-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- Schema für SQLite (AIGW-73, F7: getrennte Migrationssätze je Datenbank).
-- Frischer Schnitt: fasst die früheren portablen Migrationen 0001–0007 (AIGW-9, -43, -32, -66,
-- -46, -61, -40) zusammen und ergänzt F3 (api_key.organization_id Pflicht), F34
-- (observe_batch) und F54 (provider_credential.refs). Das Gegenstück für Postgres steht in
-- migrations/postgres/0001_init.sql – beide Sätze müssen dieselben Tabellen und Spalten haben.
--
-- Speicherung in SQLite:
--   * IDs: UUIDv7 als TEXT (Bindestrich-Form),
--   * Zeitpunkte: INTEGER Unix-Millisekunden (UTC),
--   * Geldbeträge: INTEGER Nano-Einheiten der Abrechnungswährung,
--   * Wahrheitswerte: INTEGER 0/1, strukturierte Felder: JSON als TEXT,
--   * Chiffrat/Nonce: BLOB.
-- Verweise zwischen Ressourcen laufen über IDs (Fremdschlüssel); nach außen über Namen.

-- Monoton steigende Konfigurationsversion (genau eine Zeile).
CREATE TABLE config_version (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    version    INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
INSERT INTO config_version (id, version, updated_at) VALUES (1, 0, 0);

-- Jede Admin-Änderung: wer, was, vorher/nachher (JSON ohne Geheimnisse) und Geltungsbereich
-- (Organisation/Team als Namen, vorher und nachher; AIGW-66).
CREATE TABLE audit_log (
    id                  TEXT PRIMARY KEY,
    at                  INTEGER NOT NULL,
    actor               TEXT NOT NULL,
    action              TEXT NOT NULL,
    kind                TEXT NOT NULL,
    object_id           TEXT,
    object_name         TEXT,
    config_version      INTEGER NOT NULL,
    before_json         TEXT,
    after_json          TEXT,
    organization        TEXT,
    team                TEXT,
    organization_before TEXT,
    team_before         TEXT
);
CREATE INDEX audit_log_object ON audit_log (kind, object_id);
CREATE INDEX audit_log_at_id ON audit_log (at, id);
CREATE INDEX audit_log_actor ON audit_log (actor, at);
CREATE INDEX audit_log_name ON audit_log (object_name, at);
CREATE INDEX audit_log_org ON audit_log (organization, at);
CREATE INDEX audit_log_team ON audit_log (team, at);

-- Mandanten -----------------------------------------------------------------------------------

CREATE TABLE organization (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    labels       TEXT NOT NULL DEFAULT '{}',
    display_name TEXT,
    limits       TEXT NOT NULL DEFAULT '[]',
    budget       TEXT,
    metadata     TEXT NOT NULL DEFAULT '{}',
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
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
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE (id, organization_id)            -- Ziel des Fremdschlüssels api_key (team, Org)
);
CREATE INDEX team_organization ON team (organization_id);

-- "user" ist in Postgres reserviert (gleicher Name in beiden Sätzen).
CREATE TABLE app_user (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    labels       TEXT NOT NULL DEFAULT '{}',
    oidc_issuer  TEXT,
    oidc_subject TEXT,
    email        TEXT,
    display_name TEXT,
    roles        TEXT NOT NULL DEFAULT '[]',
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    UNIQUE (oidc_issuer, oidc_subject)
);

-- F3: Organisation ist Pflicht, das Team optional. Mit Team muss die Organisation die des
-- Teams sein (zusammengesetzter Fremdschlüssel); zieht ein Team in eine andere Organisation
-- um, ziehen seine Keys mit (ON UPDATE CASCADE).
CREATE TABLE api_key (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    labels          TEXT NOT NULL DEFAULT '{}',
    prefix          TEXT NOT NULL UNIQUE,   -- 12 Zeichen, sichtbar
    key_hash        TEXT NOT NULL UNIQUE,   -- SHA-256 (hex) des vollständigen Keys
    state           TEXT NOT NULL,          -- active | revoked
    organization_id TEXT NOT NULL REFERENCES organization (id),
    team_id         TEXT REFERENCES team (id),
    user_id         TEXT REFERENCES app_user (id),
    expires_at      INTEGER,
    allowed_models  TEXT NOT NULL DEFAULT '[]',
    limits          TEXT NOT NULL DEFAULT '[]',
    budget          TEXT,
    metadata        TEXT NOT NULL DEFAULT '{}',
    last_used_at    INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    FOREIGN KEY (team_id, organization_id) REFERENCES team (id, organization_id)
        ON UPDATE CASCADE
);
CREATE INDEX api_key_organization ON api_key (organization_id);
CREATE INDEX api_key_team ON api_key (team_id);
CREATE INDEX api_key_user ON api_key (user_id);

-- Anbieter, Modelle, Routing ------------------------------------------------------------------

-- Werte AES-256-GCM-verschlüsselt; key_id = ID des Master-Keys (Rotation). refs (F54): je
-- Feld der valueFrom-Verweis (`{"env": NAME}` bzw. `{"file": PFAD}`), unverschlüsselt, nur
-- Namen – für den originalgetreuen GitOps-Export.
CREATE TABLE provider_credential (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    labels     TEXT NOT NULL DEFAULT '{}',
    provider   TEXT NOT NULL,
    fields     TEXT NOT NULL DEFAULT '[]', -- nur Feldnamen
    refs       TEXT NOT NULL DEFAULT '{}',
    key_id     TEXT NOT NULL,
    nonce      BLOB NOT NULL,              -- 12 Byte
    ciphertext BLOB NOT NULL,              -- Chiffrat + Tag
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
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
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
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
    weight         INTEGER NOT NULL DEFAULT 1,
    priority       INTEGER NOT NULL DEFAULT 0,
    rpm            INTEGER,
    tpm            INTEGER,
    enabled        INTEGER NOT NULL DEFAULT 1,
    settings       TEXT NOT NULL DEFAULT '{}',
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE INDEX deployment_model_group ON deployment (model_group_id);
CREATE INDEX deployment_credential ON deployment (credential_id);

-- Versionierte Preise; Token-Preise als Dezimaltext je 1 Mio. Tokens (JSON), Beträge in Nano.
CREATE TABLE price (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    labels        TEXT NOT NULL DEFAULT '{}',
    provider      TEXT NOT NULL DEFAULT '',  -- '' = alle Anbieter
    model         TEXT NOT NULL,
    valid_from    INTEGER NOT NULL,
    token_prices  TEXT NOT NULL DEFAULT '{}',
    per_request   INTEGER,
    per_image     INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
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
    position   INTEGER NOT NULL DEFAULT 0,
    enabled    INTEGER NOT NULL DEFAULT 1,
    config     TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX plugin_binding_target ON plugin_binding (level, target_id);

CREATE TABLE mcp_server (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    labels        TEXT NOT NULL DEFAULT '{}',
    url           TEXT NOT NULL,
    transport     TEXT NOT NULL,
    credential_id TEXT REFERENCES provider_credential (id),
    enabled       INTEGER NOT NULL DEFAULT 1,
    settings      TEXT NOT NULL DEFAULT '{}',
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

CREATE TABLE mcp_grant (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    labels        TEXT NOT NULL DEFAULT '{}',
    mcp_server_id TEXT NOT NULL REFERENCES mcp_server (id) ON DELETE CASCADE,
    subject_level TEXT NOT NULL,            -- team | api-key
    subject_id    TEXT NOT NULL,
    tools         TEXT NOT NULL DEFAULT '[]',
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE INDEX mcp_grant_subject ON mcp_grant (subject_level, subject_id);

-- Anmeldung an der Admin-API (AIGW-32) --------------------------------------------------------

-- Sessions: gespeichert wird nur SHA-256 (hex) der zufälligen Session-ID aus dem Cookie.
CREATE TABLE admin_session (
    id_hash      TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,
    groups       TEXT NOT NULL DEFAULT '[]',   -- IdP-Gruppen beim Login (JSON-Liste)
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL
);
CREATE INDEX admin_session_user ON admin_session (user_id);
CREATE INDEX admin_session_expires ON admin_session (expires_at);

-- Laufende Logins (höchstens wenige Minuten): Schlüssel = SHA-256 (hex) des state-Werts.
CREATE TABLE admin_login_state (
    state_hash    TEXT PRIMARY KEY,
    nonce         TEXT NOT NULL,
    code_verifier TEXT NOT NULL,
    return_to     TEXT NOT NULL,
    created_at    INTEGER NOT NULL
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
    expires_at   INTEGER,
    created_by   TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    last_used_at INTEGER
);

-- Schlüssel/Wert-Zustand der Anmeldung (z. B. bootstrap_disabled = Zeitpunkt + Grund).
CREATE TABLE admin_auth_state (
    name       TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Laufzeitdaten -------------------------------------------------------------------------------
-- (counter_state gibt es nur in Postgres: Zähler-Backend postgres-sync.)

-- Verbrauch je Budget-Zeitraum (Ebene + Ziel), Beträge in Nano-Einheiten (AIGW-31).
CREATE TABLE budget_usage (
    level        TEXT NOT NULL,             -- organization | team | api-key
    target_id    TEXT NOT NULL,
    period       TEXT NOT NULL,             -- day | week | month | total
    period_start INTEGER NOT NULL,
    amount       INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (level, target_id, period, period_start)
);

-- Deduplizierung von Ereignis-Webhooks im Cluster (AIGW-61).
CREATE TABLE event_dedup (
    dedup_key  TEXT PRIMARY KEY,
    event_type TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX event_dedup_expires ON event_dedup (expires_at);

-- Warmstart des semantischen Caches (AIGW-40): Vektor und Antwort gemeinsam verschlüsselt
-- (AES-256-GCM, AAD = id). Das Backend pgvector legt seine Tabelle selbst an.
CREATE TABLE cache_vector (
    id            TEXT PRIMARY KEY,         -- BLAKE3 (hex) des exakten Cache-Schlüssels
    partition_key TEXT NOT NULL,
    dims          INTEGER NOT NULL,
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    key_id        TEXT NOT NULL,
    nonce         BLOB NOT NULL,
    ciphertext    BLOB NOT NULL             -- f32-LE-Vektor ‖ Antwort-JSON, inkl. Tag
);
CREATE INDEX cache_vector_expires ON cache_vector (expires_at);

-- Request-Log (AIGW-43, AIGW-46) --------------------------------------------------------------

-- Rohlog je Anfrage (gebündelt geschrieben, befristet).
CREATE TABLE request_log (
    id                 TEXT PRIMARY KEY,    -- UUIDv7 = x-aigw-request-id
    at                 INTEGER NOT NULL,
    api_key_id         TEXT,
    team_id            TEXT,
    organization_id    TEXT,
    endpoint           TEXT NOT NULL,
    model_requested    TEXT,
    model_group        TEXT,
    deployment_id      TEXT,
    provider           TEXT,
    provider_model     TEXT,
    status             INTEGER NOT NULL,
    error_code         TEXT,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cached_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
    usage_estimated    INTEGER NOT NULL DEFAULT 0,
    cost               INTEGER NOT NULL DEFAULT 0, -- Nano-Einheiten
    latency_ms         INTEGER NOT NULL DEFAULT 0,
    ttft_ms            INTEGER,
    cache              TEXT,                -- hit-exact | hit-semantic | miss
    stream             INTEGER NOT NULL DEFAULT 0,
    client_aborted     INTEGER NOT NULL DEFAULT 0,
    client_ip          TEXT,
    metadata           TEXT NOT NULL DEFAULT '{}',
    attempts           TEXT,                -- JSON-Array der Upstream-Versuche
    client_request_id  TEXT,
    parent_request_id  TEXT                 -- F191: auslösende Anfrage (z. B. Cache-Embedding)
);
CREATE INDEX request_log_at ON request_log (at);
CREATE INDEX request_log_key_at ON request_log (api_key_id, at);
CREATE INDEX request_log_team_at ON request_log (team_id, at);
CREATE INDEX request_log_org_at ON request_log (organization_id, at);
CREATE INDEX request_log_model_at ON request_log (model_group, at);
CREATE INDEX request_log_deployment_at ON request_log (deployment_id, at);
CREATE INDEX request_log_client_request_id ON request_log (client_request_id);
CREATE INDEX request_log_parent ON request_log (parent_request_id);

-- Payload in eigener Tabelle: eigene Frist, schmale request_log-Zeilen.
CREATE TABLE request_payload (
    id      TEXT PRIMARY KEY,               -- = request_log.id (kein FK: unabhängig löschbar)
    at      INTEGER NOT NULL,
    mode    TEXT NOT NULL,                  -- metadata | masked | full
    payload TEXT NOT NULL                   -- JSON
);
CREATE INDEX request_payload_at ON request_payload (at);

-- Minuten-Rollups ('' statt NULL, da Teil des PK). Histogramme als feste Spalten (additiver
-- UPSERT ohne Read-Modify-Write); Zählung je Bucket, Grenzen in ms wie
-- aigw_store::observe::LATENCY_BUCKETS_MS.
CREATE TABLE usage_rollup (
    bucket_start       INTEGER NOT NULL,    -- Minutenbeginn (Unix-ms)
    api_key_id         TEXT NOT NULL DEFAULT '',
    team_id            TEXT NOT NULL DEFAULT '',
    organization_id    TEXT NOT NULL DEFAULT '',
    model_group        TEXT NOT NULL DEFAULT '',
    deployment_id      TEXT NOT NULL DEFAULT '',
    requests           INTEGER NOT NULL DEFAULT 0,
    errors             INTEGER NOT NULL DEFAULT 0,
    cache_hits         INTEGER NOT NULL DEFAULT 0,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cached_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
    cost               INTEGER NOT NULL DEFAULT 0,
    latency_ms_sum     INTEGER NOT NULL DEFAULT 0,
    estimated          INTEGER NOT NULL DEFAULT 0,
    aborted            INTEGER NOT NULL DEFAULT 0,
    ttft_ms_sum        INTEGER NOT NULL DEFAULT 0,
    ttft_count         INTEGER NOT NULL DEFAULT 0,
    lat_le_25          INTEGER NOT NULL DEFAULT 0,
    lat_le_50          INTEGER NOT NULL DEFAULT 0,
    lat_le_100         INTEGER NOT NULL DEFAULT 0,
    lat_le_250         INTEGER NOT NULL DEFAULT 0,
    lat_le_500         INTEGER NOT NULL DEFAULT 0,
    lat_le_1000        INTEGER NOT NULL DEFAULT 0,
    lat_le_2500        INTEGER NOT NULL DEFAULT 0,
    lat_le_5000        INTEGER NOT NULL DEFAULT 0,
    lat_le_10000       INTEGER NOT NULL DEFAULT 0,
    lat_le_30000       INTEGER NOT NULL DEFAULT 0,
    lat_le_60000       INTEGER NOT NULL DEFAULT 0,
    lat_inf            INTEGER NOT NULL DEFAULT 0,
    ttft_le_25         INTEGER NOT NULL DEFAULT 0,
    ttft_le_50         INTEGER NOT NULL DEFAULT 0,
    ttft_le_100        INTEGER NOT NULL DEFAULT 0,
    ttft_le_250        INTEGER NOT NULL DEFAULT 0,
    ttft_le_500        INTEGER NOT NULL DEFAULT 0,
    ttft_le_1000       INTEGER NOT NULL DEFAULT 0,
    ttft_le_2500       INTEGER NOT NULL DEFAULT 0,
    ttft_le_5000       INTEGER NOT NULL DEFAULT 0,
    ttft_le_10000      INTEGER NOT NULL DEFAULT 0,
    ttft_le_30000      INTEGER NOT NULL DEFAULT 0,
    ttft_le_60000      INTEGER NOT NULL DEFAULT 0,
    ttft_inf           INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id)
);
CREATE INDEX usage_rollup_team ON usage_rollup (team_id, bucket_start);
CREATE INDEX usage_rollup_model ON usage_rollup (model_group, bucket_start);
CREATE INDEX usage_rollup_deployment ON usage_rollup (deployment_id, bucket_start);
CREATE INDEX usage_rollup_key ON usage_rollup (api_key_id, bucket_start);
CREATE INDEX usage_rollup_org ON usage_rollup (organization_id, bucket_start);

-- Stunden-Rollups: gleiche Spalten, Bucket = Stundenbeginn (UTC).
CREATE TABLE usage_rollup_hour (
    bucket_start       INTEGER NOT NULL,
    api_key_id         TEXT NOT NULL DEFAULT '',
    team_id            TEXT NOT NULL DEFAULT '',
    organization_id    TEXT NOT NULL DEFAULT '',
    model_group        TEXT NOT NULL DEFAULT '',
    deployment_id      TEXT NOT NULL DEFAULT '',
    requests           INTEGER NOT NULL DEFAULT 0,
    errors             INTEGER NOT NULL DEFAULT 0,
    cache_hits         INTEGER NOT NULL DEFAULT 0,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cached_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
    cost               INTEGER NOT NULL DEFAULT 0,
    latency_ms_sum     INTEGER NOT NULL DEFAULT 0,
    estimated          INTEGER NOT NULL DEFAULT 0,
    aborted            INTEGER NOT NULL DEFAULT 0,
    ttft_ms_sum        INTEGER NOT NULL DEFAULT 0,
    ttft_count         INTEGER NOT NULL DEFAULT 0,
    lat_le_25          INTEGER NOT NULL DEFAULT 0,
    lat_le_50          INTEGER NOT NULL DEFAULT 0,
    lat_le_100         INTEGER NOT NULL DEFAULT 0,
    lat_le_250         INTEGER NOT NULL DEFAULT 0,
    lat_le_500         INTEGER NOT NULL DEFAULT 0,
    lat_le_1000        INTEGER NOT NULL DEFAULT 0,
    lat_le_2500        INTEGER NOT NULL DEFAULT 0,
    lat_le_5000        INTEGER NOT NULL DEFAULT 0,
    lat_le_10000       INTEGER NOT NULL DEFAULT 0,
    lat_le_30000       INTEGER NOT NULL DEFAULT 0,
    lat_le_60000       INTEGER NOT NULL DEFAULT 0,
    lat_inf            INTEGER NOT NULL DEFAULT 0,
    ttft_le_25         INTEGER NOT NULL DEFAULT 0,
    ttft_le_50         INTEGER NOT NULL DEFAULT 0,
    ttft_le_100        INTEGER NOT NULL DEFAULT 0,
    ttft_le_250        INTEGER NOT NULL DEFAULT 0,
    ttft_le_500        INTEGER NOT NULL DEFAULT 0,
    ttft_le_1000       INTEGER NOT NULL DEFAULT 0,
    ttft_le_2500       INTEGER NOT NULL DEFAULT 0,
    ttft_le_5000       INTEGER NOT NULL DEFAULT 0,
    ttft_le_10000      INTEGER NOT NULL DEFAULT 0,
    ttft_le_30000      INTEGER NOT NULL DEFAULT 0,
    ttft_le_60000      INTEGER NOT NULL DEFAULT 0,
    ttft_inf           INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id)
);
CREATE INDEX usage_rollup_hour_team ON usage_rollup_hour (team_id, bucket_start);
CREATE INDEX usage_rollup_hour_key ON usage_rollup_hour (api_key_id, bucket_start);
CREATE INDEX usage_rollup_hour_org ON usage_rollup_hour (organization_id, bucket_start);
CREATE INDEX usage_rollup_hour_model ON usage_rollup_hour (model_group, bucket_start);
CREATE INDEX usage_rollup_hour_deployment ON usage_rollup_hour (deployment_id, bucket_start);

-- F34: angewendete Writer-Batches. Ein Batch (Rohlogs, Payloads, Rollup-Deltas) wird nur
-- angewendet, wenn seine ID hier noch fehlt – Wiederholungen nach unklarem Commit-Ausgang
-- zählen Rollups nicht doppelt. Aufbewahrung kurz (Retention, Standard 7 Tage).
CREATE TABLE observe_batch (
    batch_id   TEXT PRIMARY KEY,
    applied_at INTEGER NOT NULL
);
CREATE INDEX observe_batch_applied ON observe_batch (applied_at);
