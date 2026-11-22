-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- Schema für Postgres (AIGW-73, F7: getrennte Migrationssätze je Datenbank, native Typen).
-- Frischer Schnitt: fasst die früheren portablen Migrationen 0001–0007 (AIGW-9, -43, -32, -66,
-- -46, -61, -40) zusammen und ergänzt F3 (api_key.organization_id Pflicht), F34
-- (observe_batch) und F54 (provider_credential.refs). Gegenstück für SQLite:
-- migrations/sqlite/0001_init.sql – beide Sätze müssen dieselben Tabellen und Spalten haben.
--
-- Native Typen:
--   * IDs: UUID (v7), Zeitpunkte: TIMESTAMPTZ (auf Millisekunden gekürzt geschrieben),
--   * strukturierte Felder: JSONB, Wahrheitswerte: BOOLEAN, Chiffrat/Nonce: BYTEA,
--   * Geldbeträge: BIGINT in Nano-Einheiten (exakt, i64; schneller zu summieren als NUMERIC),
--   * Namen mit Sortierfolge "C" (Bytefolge wie in Rust, stabile Keyset-Pagination).
-- Verweise zwischen Ressourcen laufen über IDs (Fremdschlüssel); nach außen über Namen.

-- Monoton steigende Konfigurationsversion (genau eine Zeile).
CREATE TABLE config_version (
    id         SMALLINT PRIMARY KEY CHECK (id = 1),
    version    BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
INSERT INTO config_version (id, version, updated_at)
VALUES (1, 0, TIMESTAMPTZ '1970-01-01 00:00:00+00');

-- Jede Admin-Änderung: wer, was, vorher/nachher (JSON ohne Geheimnisse) und Geltungsbereich
-- (Organisation/Team als Namen, vorher und nachher; AIGW-66).
CREATE TABLE audit_log (
    id                  UUID PRIMARY KEY,
    at                  TIMESTAMPTZ NOT NULL,
    actor               TEXT NOT NULL,
    action              TEXT NOT NULL,
    kind                TEXT NOT NULL,
    object_id           UUID,
    object_name         TEXT,
    config_version      BIGINT NOT NULL,
    before_json         JSONB,
    after_json          JSONB,
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
    id           UUID PRIMARY KEY,
    name         TEXT COLLATE "C" NOT NULL UNIQUE,
    labels       JSONB NOT NULL DEFAULT '{}',
    display_name TEXT,
    limits       JSONB NOT NULL DEFAULT '[]',
    budget       JSONB,
    metadata     JSONB NOT NULL DEFAULT '{}',
    created_at   TIMESTAMPTZ NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL
);

CREATE TABLE team (
    id              UUID PRIMARY KEY,
    name            TEXT COLLATE "C" NOT NULL UNIQUE,
    labels          JSONB NOT NULL DEFAULT '{}',
    organization_id UUID NOT NULL REFERENCES organization (id),
    display_name    TEXT,
    allowed_models  JSONB NOT NULL DEFAULT '[]',
    limits          JSONB NOT NULL DEFAULT '[]',
    budget          JSONB,
    metadata        JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL,
    UNIQUE (id, organization_id)            -- Ziel des Fremdschlüssels api_key (Team, Org)
);
CREATE INDEX team_organization ON team (organization_id);

-- "user" ist in Postgres reserviert.
CREATE TABLE app_user (
    id           UUID PRIMARY KEY,
    name         TEXT COLLATE "C" NOT NULL UNIQUE,
    labels       JSONB NOT NULL DEFAULT '{}',
    oidc_issuer  TEXT,
    oidc_subject TEXT,
    email        TEXT,
    display_name TEXT,
    roles        JSONB NOT NULL DEFAULT '[]',
    created_at   TIMESTAMPTZ NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL,
    UNIQUE (oidc_issuer, oidc_subject)
);

-- F3: Organisation ist Pflicht, das Team optional. Mit Team muss die Organisation die des
-- Teams sein (zusammengesetzter Fremdschlüssel); zieht ein Team in eine andere Organisation
-- um, ziehen seine Keys mit (ON UPDATE CASCADE).
CREATE TABLE api_key (
    id              UUID PRIMARY KEY,
    name            TEXT COLLATE "C" NOT NULL UNIQUE,
    labels          JSONB NOT NULL DEFAULT '{}',
    prefix          TEXT NOT NULL UNIQUE,   -- 12 Zeichen, sichtbar
    key_hash        TEXT NOT NULL UNIQUE,   -- SHA-256 (hex) des vollständigen Keys
    state           TEXT NOT NULL,          -- active | revoked
    organization_id UUID NOT NULL REFERENCES organization (id),
    team_id         UUID REFERENCES team (id),
    user_id         UUID REFERENCES app_user (id),
    expires_at      TIMESTAMPTZ,
    allowed_models  JSONB NOT NULL DEFAULT '[]',
    limits          JSONB NOT NULL DEFAULT '[]',
    budget          JSONB,
    metadata        JSONB NOT NULL DEFAULT '{}',
    last_used_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL,
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
    id         UUID PRIMARY KEY,
    name       TEXT COLLATE "C" NOT NULL UNIQUE,
    labels     JSONB NOT NULL DEFAULT '{}',
    provider   TEXT NOT NULL,
    fields     JSONB NOT NULL DEFAULT '[]', -- nur Feldnamen
    refs       JSONB NOT NULL DEFAULT '{}',
    key_id     TEXT NOT NULL,
    nonce      BYTEA NOT NULL,             -- 12 Byte
    ciphertext BYTEA NOT NULL,             -- Chiffrat + Tag
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE model_group (
    id         UUID PRIMARY KEY,
    name       TEXT COLLATE "C" NOT NULL UNIQUE,
    labels     JSONB NOT NULL DEFAULT '{}',
    aliases    JSONB NOT NULL DEFAULT '[]',
    routing    TEXT NOT NULL,
    fallbacks  JSONB NOT NULL DEFAULT '[]',
    limits     JSONB NOT NULL DEFAULT '[]',
    settings   JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

-- Name und Aliasse aller Modellgruppen in einem Namensraum (Eindeutigkeit über Gruppen hinweg).
CREATE TABLE model_alias (
    alias          TEXT COLLATE "C" PRIMARY KEY,
    model_group_id UUID NOT NULL REFERENCES model_group (id) ON DELETE CASCADE
);
CREATE INDEX model_alias_group ON model_alias (model_group_id);

CREATE TABLE deployment (
    id             UUID PRIMARY KEY,
    name           TEXT COLLATE "C" NOT NULL UNIQUE,
    labels         JSONB NOT NULL DEFAULT '{}',
    model_group_id UUID NOT NULL REFERENCES model_group (id),
    provider       TEXT NOT NULL,
    endpoint       TEXT,
    provider_model TEXT NOT NULL,
    credential_id  UUID REFERENCES provider_credential (id),
    weight         BIGINT NOT NULL DEFAULT 1,
    priority       BIGINT NOT NULL DEFAULT 0,
    rpm            BIGINT,
    tpm            BIGINT,
    enabled        BOOLEAN NOT NULL DEFAULT TRUE,
    settings       JSONB NOT NULL DEFAULT '{}',
    created_at     TIMESTAMPTZ NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL
);
CREATE INDEX deployment_model_group ON deployment (model_group_id);
CREATE INDEX deployment_credential ON deployment (credential_id);

-- Versionierte Preise; Token-Preise als Dezimaltext je 1 Mio. Tokens (JSONB), Beträge in Nano.
CREATE TABLE price (
    id            UUID PRIMARY KEY,
    name          TEXT COLLATE "C" NOT NULL UNIQUE,
    labels        JSONB NOT NULL DEFAULT '{}',
    provider      TEXT NOT NULL DEFAULT '',  -- '' = alle Anbieter
    model         TEXT NOT NULL,
    valid_from    TIMESTAMPTZ NOT NULL,
    token_prices  JSONB NOT NULL DEFAULT '{}',
    per_request   BIGINT,
    per_image     BIGINT,
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL,
    UNIQUE (provider, model, valid_from)
);

-- Plugins und MCP -----------------------------------------------------------------------------

-- target_id verweist je nach level auf organization/team/api_key/model_group (polymorph,
-- daher ohne Fremdschlüssel; das Löschen des Ziels löscht Bindungen im Repository mit).
CREATE TABLE plugin_binding (
    id         UUID PRIMARY KEY,
    name       TEXT COLLATE "C" NOT NULL UNIQUE,
    labels     JSONB NOT NULL DEFAULT '{}',
    level      TEXT NOT NULL,
    target_id  UUID,
    plugin     TEXT NOT NULL,
    position   BIGINT NOT NULL DEFAULT 0,
    enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    config     JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX plugin_binding_target ON plugin_binding (level, target_id);

CREATE TABLE mcp_server (
    id            UUID PRIMARY KEY,
    name          TEXT COLLATE "C" NOT NULL UNIQUE,
    labels        JSONB NOT NULL DEFAULT '{}',
    url           TEXT NOT NULL,
    transport     TEXT NOT NULL,
    credential_id UUID REFERENCES provider_credential (id),
    enabled       BOOLEAN NOT NULL DEFAULT TRUE,
    settings      JSONB NOT NULL DEFAULT '{}',
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL
);

CREATE TABLE mcp_grant (
    id            UUID PRIMARY KEY,
    name          TEXT COLLATE "C" NOT NULL UNIQUE,
    labels        JSONB NOT NULL DEFAULT '{}',
    mcp_server_id UUID NOT NULL REFERENCES mcp_server (id) ON DELETE CASCADE,
    subject_level TEXT NOT NULL,            -- team | api-key
    subject_id    UUID NOT NULL,
    tools         JSONB NOT NULL DEFAULT '[]',
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL
);
CREATE INDEX mcp_grant_subject ON mcp_grant (subject_level, subject_id);

-- Anmeldung an der Admin-API (AIGW-32) --------------------------------------------------------

-- Sessions: gespeichert wird nur SHA-256 (hex) der zufälligen Session-ID aus dem Cookie.
CREATE TABLE admin_session (
    id_hash      TEXT PRIMARY KEY,
    user_id      UUID NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,
    groups       JSONB NOT NULL DEFAULT '[]',  -- IdP-Gruppen beim Login
    created_at   TIMESTAMPTZ NOT NULL,
    last_seen_at TIMESTAMPTZ NOT NULL,
    expires_at   TIMESTAMPTZ NOT NULL
);
CREATE INDEX admin_session_user ON admin_session (user_id);
CREATE INDEX admin_session_expires ON admin_session (expires_at);

-- Laufende Logins (höchstens wenige Minuten): Schlüssel = SHA-256 (hex) des state-Werts.
CREATE TABLE admin_login_state (
    state_hash    TEXT PRIMARY KEY,
    nonce         TEXT NOT NULL,
    code_verifier TEXT NOT NULL,
    return_to     TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL
);
CREATE INDEX admin_login_state_created ON admin_login_state (created_at);

-- Admin-API-Keys (sk-aigw-admin-<Präfix>-<Geheimnis>), getrennt von den Keys der Datenebene.
CREATE TABLE admin_api_key (
    id           UUID PRIMARY KEY,
    name         TEXT COLLATE "C" NOT NULL UNIQUE,
    prefix       TEXT NOT NULL UNIQUE,
    key_hash     TEXT NOT NULL UNIQUE,         -- SHA-256 (hex) des vollständigen Keys
    description  TEXT,
    roles        JSONB NOT NULL DEFAULT '[]',  -- Liste von RoleBinding
    expires_at   TIMESTAMPTZ,
    created_by   TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ
);

-- Schlüssel/Wert-Zustand der Anmeldung (z. B. bootstrap_disabled = Zeitpunkt + Grund).
CREATE TABLE admin_auth_state (
    name       TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

-- Laufzeitdaten -------------------------------------------------------------------------------

-- Nur für counters.backend = postgres-sync (aigw-counters, eigener PgPool): Deltas je Instanz
-- und Fenster. window_start ist der Bucket-Schlüssel des Zählers (ms), daher BIGINT.
CREATE TABLE counter_state (
    counter_key  TEXT NOT NULL,
    window_start BIGINT NOT NULL,
    instance_id  TEXT NOT NULL,
    value        BIGINT NOT NULL,
    updated_at   BIGINT NOT NULL,
    PRIMARY KEY (counter_key, window_start, instance_id)
);
CREATE INDEX counter_state_window ON counter_state (window_start);

-- Verbrauch je Budget-Zeitraum (Ebene + Ziel), Beträge in Nano-Einheiten (AIGW-31).
CREATE TABLE budget_usage (
    level        TEXT NOT NULL,             -- organization | team | api-key
    target_id    UUID NOT NULL,
    period       TEXT NOT NULL,             -- day | week | month | total
    period_start TIMESTAMPTZ NOT NULL,
    amount       BIGINT NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (level, target_id, period, period_start)
);

-- Deduplizierung von Ereignis-Webhooks im Cluster (AIGW-61).
CREATE TABLE event_dedup (
    dedup_key  TEXT PRIMARY KEY,
    event_type TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX event_dedup_expires ON event_dedup (expires_at);

-- Warmstart des semantischen Caches (AIGW-40): Vektor und Antwort gemeinsam verschlüsselt
-- (AES-256-GCM, AAD = id). Das Backend pgvector legt seine Tabelle selbst an.
CREATE TABLE cache_vector (
    id            TEXT PRIMARY KEY,         -- BLAKE3 (hex) des exakten Cache-Schlüssels
    partition_key TEXT NOT NULL,
    dims          BIGINT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL,
    expires_at    TIMESTAMPTZ NOT NULL,
    key_id        TEXT NOT NULL,
    nonce         BYTEA NOT NULL,
    ciphertext    BYTEA NOT NULL            -- f32-LE-Vektor ‖ Antwort-JSON, inkl. Tag
);
CREATE INDEX cache_vector_expires ON cache_vector (expires_at);

-- Request-Log (AIGW-43, AIGW-46) --------------------------------------------------------------

-- Rohlog je Anfrage (gebündelt geschrieben, befristet).
CREATE TABLE request_log (
    id                 UUID PRIMARY KEY,    -- UUIDv7 = x-aigw-request-id
    at                 TIMESTAMPTZ NOT NULL,
    api_key_id         UUID,
    team_id            UUID,
    organization_id    UUID,
    endpoint           TEXT NOT NULL,
    model_requested    TEXT,
    model_group        TEXT,
    deployment_id      UUID,
    provider           TEXT,
    provider_model     TEXT,
    status             BIGINT NOT NULL,
    error_code         TEXT,
    input_tokens       BIGINT NOT NULL DEFAULT 0,
    output_tokens      BIGINT NOT NULL DEFAULT 0,
    cached_tokens      BIGINT NOT NULL DEFAULT 0,
    cache_write_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens   BIGINT NOT NULL DEFAULT 0,
    usage_estimated    BOOLEAN NOT NULL DEFAULT FALSE,
    cost               BIGINT NOT NULL DEFAULT 0, -- Nano-Einheiten
    latency_ms         BIGINT NOT NULL DEFAULT 0,
    ttft_ms            BIGINT,
    cache              TEXT,                -- hit-exact | hit-semantic | miss
    stream             BOOLEAN NOT NULL DEFAULT FALSE,
    client_aborted     BOOLEAN NOT NULL DEFAULT FALSE,
    client_ip          TEXT,
    metadata           JSONB NOT NULL DEFAULT '{}',
    attempts           JSONB,               -- Array der Upstream-Versuche
    client_request_id  TEXT,
    parent_request_id  UUID                 -- F191: auslösende Anfrage (z. B. Cache-Embedding)
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
    id      UUID PRIMARY KEY,               -- = request_log.id (kein FK: unabhängig löschbar)
    at      TIMESTAMPTZ NOT NULL,
    mode    TEXT NOT NULL,                  -- metadata | masked | full
    payload JSONB NOT NULL
);
CREATE INDEX request_payload_at ON request_payload (at);

-- Minuten-Rollups. Schlüssel ohne Wert = Null-UUID bzw. '' (Teil des PK, daher nicht NULL).
-- Histogramme als feste Spalten (additiver UPSERT ohne Read-Modify-Write); Zählung je Bucket,
-- Grenzen in ms wie aigw_store::observe::LATENCY_BUCKETS_MS.
CREATE TABLE usage_rollup (
    bucket_start       TIMESTAMPTZ NOT NULL,
    api_key_id         UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    team_id            UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    organization_id    UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    model_group        TEXT NOT NULL DEFAULT '',
    deployment_id      UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    requests           BIGINT NOT NULL DEFAULT 0,
    errors             BIGINT NOT NULL DEFAULT 0,
    cache_hits         BIGINT NOT NULL DEFAULT 0,
    input_tokens       BIGINT NOT NULL DEFAULT 0,
    output_tokens      BIGINT NOT NULL DEFAULT 0,
    cached_tokens      BIGINT NOT NULL DEFAULT 0,
    cache_write_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens   BIGINT NOT NULL DEFAULT 0,
    cost               BIGINT NOT NULL DEFAULT 0,
    latency_ms_sum     BIGINT NOT NULL DEFAULT 0,
    estimated          BIGINT NOT NULL DEFAULT 0,
    aborted            BIGINT NOT NULL DEFAULT 0,
    ttft_ms_sum        BIGINT NOT NULL DEFAULT 0,
    ttft_count         BIGINT NOT NULL DEFAULT 0,
    lat_le_25          BIGINT NOT NULL DEFAULT 0,
    lat_le_50          BIGINT NOT NULL DEFAULT 0,
    lat_le_100         BIGINT NOT NULL DEFAULT 0,
    lat_le_250         BIGINT NOT NULL DEFAULT 0,
    lat_le_500         BIGINT NOT NULL DEFAULT 0,
    lat_le_1000        BIGINT NOT NULL DEFAULT 0,
    lat_le_2500        BIGINT NOT NULL DEFAULT 0,
    lat_le_5000        BIGINT NOT NULL DEFAULT 0,
    lat_le_10000       BIGINT NOT NULL DEFAULT 0,
    lat_le_30000       BIGINT NOT NULL DEFAULT 0,
    lat_le_60000       BIGINT NOT NULL DEFAULT 0,
    lat_inf            BIGINT NOT NULL DEFAULT 0,
    ttft_le_25         BIGINT NOT NULL DEFAULT 0,
    ttft_le_50         BIGINT NOT NULL DEFAULT 0,
    ttft_le_100        BIGINT NOT NULL DEFAULT 0,
    ttft_le_250        BIGINT NOT NULL DEFAULT 0,
    ttft_le_500        BIGINT NOT NULL DEFAULT 0,
    ttft_le_1000       BIGINT NOT NULL DEFAULT 0,
    ttft_le_2500       BIGINT NOT NULL DEFAULT 0,
    ttft_le_5000       BIGINT NOT NULL DEFAULT 0,
    ttft_le_10000      BIGINT NOT NULL DEFAULT 0,
    ttft_le_30000      BIGINT NOT NULL DEFAULT 0,
    ttft_le_60000      BIGINT NOT NULL DEFAULT 0,
    ttft_inf           BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id)
);
CREATE INDEX usage_rollup_team ON usage_rollup (team_id, bucket_start);
CREATE INDEX usage_rollup_model ON usage_rollup (model_group, bucket_start);
CREATE INDEX usage_rollup_deployment ON usage_rollup (deployment_id, bucket_start);
CREATE INDEX usage_rollup_key ON usage_rollup (api_key_id, bucket_start);
CREATE INDEX usage_rollup_org ON usage_rollup (organization_id, bucket_start);

-- Stunden-Rollups: gleiche Spalten, Bucket = Stundenbeginn (UTC).
CREATE TABLE usage_rollup_hour (
    bucket_start       TIMESTAMPTZ NOT NULL,
    api_key_id         UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    team_id            UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    organization_id    UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    model_group        TEXT NOT NULL DEFAULT '',
    deployment_id      UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    requests           BIGINT NOT NULL DEFAULT 0,
    errors             BIGINT NOT NULL DEFAULT 0,
    cache_hits         BIGINT NOT NULL DEFAULT 0,
    input_tokens       BIGINT NOT NULL DEFAULT 0,
    output_tokens      BIGINT NOT NULL DEFAULT 0,
    cached_tokens      BIGINT NOT NULL DEFAULT 0,
    cache_write_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens   BIGINT NOT NULL DEFAULT 0,
    cost               BIGINT NOT NULL DEFAULT 0,
    latency_ms_sum     BIGINT NOT NULL DEFAULT 0,
    estimated          BIGINT NOT NULL DEFAULT 0,
    aborted            BIGINT NOT NULL DEFAULT 0,
    ttft_ms_sum        BIGINT NOT NULL DEFAULT 0,
    ttft_count         BIGINT NOT NULL DEFAULT 0,
    lat_le_25          BIGINT NOT NULL DEFAULT 0,
    lat_le_50          BIGINT NOT NULL DEFAULT 0,
    lat_le_100         BIGINT NOT NULL DEFAULT 0,
    lat_le_250         BIGINT NOT NULL DEFAULT 0,
    lat_le_500         BIGINT NOT NULL DEFAULT 0,
    lat_le_1000        BIGINT NOT NULL DEFAULT 0,
    lat_le_2500        BIGINT NOT NULL DEFAULT 0,
    lat_le_5000        BIGINT NOT NULL DEFAULT 0,
    lat_le_10000       BIGINT NOT NULL DEFAULT 0,
    lat_le_30000       BIGINT NOT NULL DEFAULT 0,
    lat_le_60000       BIGINT NOT NULL DEFAULT 0,
    lat_inf            BIGINT NOT NULL DEFAULT 0,
    ttft_le_25         BIGINT NOT NULL DEFAULT 0,
    ttft_le_50         BIGINT NOT NULL DEFAULT 0,
    ttft_le_100        BIGINT NOT NULL DEFAULT 0,
    ttft_le_250        BIGINT NOT NULL DEFAULT 0,
    ttft_le_500        BIGINT NOT NULL DEFAULT 0,
    ttft_le_1000       BIGINT NOT NULL DEFAULT 0,
    ttft_le_2500       BIGINT NOT NULL DEFAULT 0,
    ttft_le_5000       BIGINT NOT NULL DEFAULT 0,
    ttft_le_10000      BIGINT NOT NULL DEFAULT 0,
    ttft_le_30000      BIGINT NOT NULL DEFAULT 0,
    ttft_le_60000      BIGINT NOT NULL DEFAULT 0,
    ttft_inf           BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id)
);CREATE INDEX usage_rollup_hour_team ON usage_rollup_hour (team_id, bucket_start);
CREATE INDEX usage_rollup_hour_key ON usage_rollup_hour (api_key_id, bucket_start);
CREATE INDEX usage_rollup_hour_org ON usage_rollup_hour (organization_id, bucket_start);
CREATE INDEX usage_rollup_hour_model ON usage_rollup_hour (model_group, bucket_start);
CREATE INDEX usage_rollup_hour_deployment ON usage_rollup_hour (deployment_id, bucket_start);

-- F34: angewendete Writer-Batches. Ein Batch (Rohlogs, Payloads, Rollup-Deltas) wird nur
-- angewendet, wenn seine ID hier noch fehlt – Wiederholungen nach unklarem Commit-Ausgang
-- zählen Rollups nicht doppelt. Aufbewahrung kurz (Retention, Standard 7 Tage).
CREATE TABLE observe_batch (
    batch_id   UUID PRIMARY KEY,
    applied_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX observe_batch_applied ON observe_batch (applied_at);
