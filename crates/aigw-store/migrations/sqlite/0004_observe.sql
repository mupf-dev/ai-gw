-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-76 Zähler & Beobachtung (SQLite; Gegenstück in migrations/postgres/0004_observe.sql):
--   * F30/F185: payload_mode und expose_reasoning an Organisation, Team und API-Key (Vererbung
--     Key → Team → Organisation → observe.payload bzw. settings.exposeReasoning),
--   * F47: request_log.error_class (feine Fehlerklasse) und error_class als weitere
--     Schlüsselspalte der Minuten-/Stunden-Rollups (Dashboard-Dimension; '' = kein Fehler),
--   * F100: request_log.trace_id (Verknüpfung Log ↔ Trace),
--   * F161: request_log.guardrails (Zähler der Guardrail-Erkennungen als JSON, ohne Inhalte).

-- F30/F185: Vererbbare Einstellungen (NULL = nicht gesetzt, erbt).
ALTER TABLE organization ADD COLUMN payload_mode TEXT;
ALTER TABLE organization ADD COLUMN expose_reasoning INTEGER;
ALTER TABLE team ADD COLUMN payload_mode TEXT;
ALTER TABLE team ADD COLUMN expose_reasoning INTEGER;
ALTER TABLE api_key ADD COLUMN payload_mode TEXT;
ALTER TABLE api_key ADD COLUMN expose_reasoning INTEGER;

-- F47/F100/F161: Rohlog.
ALTER TABLE request_log ADD COLUMN error_class TEXT;
ALTER TABLE request_log ADD COLUMN trace_id TEXT;
ALTER TABLE request_log ADD COLUMN guardrails TEXT;
CREATE INDEX request_log_error_class_at ON request_log (error_class, at)
    WHERE error_class IS NOT NULL;
CREATE INDEX request_log_trace ON request_log (trace_id) WHERE trace_id IS NOT NULL;

-- F47: error_class als Schlüsselspalte der Rollups. SQLite kann den Primärschlüssel nicht
-- ändern: Tabellen neu anlegen, umkopieren (bestehende Zeilen: error_class = ''), Indizes neu.

CREATE TABLE usage_rollup_new (
    bucket_start       INTEGER NOT NULL,    -- Minutenbeginn (Unix-ms)
    api_key_id         TEXT NOT NULL DEFAULT '',
    team_id            TEXT NOT NULL DEFAULT '',
    organization_id    TEXT NOT NULL DEFAULT '',
    model_group        TEXT NOT NULL DEFAULT '',
    deployment_id      TEXT NOT NULL DEFAULT '',
    error_class        TEXT NOT NULL DEFAULT '',
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
    PRIMARY KEY (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id,
                 error_class)
);
INSERT INTO usage_rollup_new (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id, requests, errors, cache_hits, input_tokens, output_tokens, cached_tokens, cache_write_tokens, reasoning_tokens, cost, latency_ms_sum, estimated, aborted, ttft_ms_sum, ttft_count, lat_le_25, lat_le_50, lat_le_100, lat_le_250, lat_le_500, lat_le_1000, lat_le_2500, lat_le_5000, lat_le_10000, lat_le_30000, lat_le_60000, lat_inf, ttft_le_25, ttft_le_50, ttft_le_100, ttft_le_250, ttft_le_500, ttft_le_1000, ttft_le_2500, ttft_le_5000, ttft_le_10000, ttft_le_30000, ttft_le_60000, ttft_inf)
    SELECT bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id, requests, errors, cache_hits, input_tokens, output_tokens, cached_tokens, cache_write_tokens, reasoning_tokens, cost, latency_ms_sum, estimated, aborted, ttft_ms_sum, ttft_count, lat_le_25, lat_le_50, lat_le_100, lat_le_250, lat_le_500, lat_le_1000, lat_le_2500, lat_le_5000, lat_le_10000, lat_le_30000, lat_le_60000, lat_inf, ttft_le_25, ttft_le_50, ttft_le_100, ttft_le_250, ttft_le_500, ttft_le_1000, ttft_le_2500, ttft_le_5000, ttft_le_10000, ttft_le_30000, ttft_le_60000, ttft_inf FROM usage_rollup;
DROP TABLE usage_rollup;
ALTER TABLE usage_rollup_new RENAME TO usage_rollup;
CREATE INDEX usage_rollup_team ON usage_rollup (team_id, bucket_start);
CREATE INDEX usage_rollup_model ON usage_rollup (model_group, bucket_start);
CREATE INDEX usage_rollup_deployment ON usage_rollup (deployment_id, bucket_start);
CREATE INDEX usage_rollup_key ON usage_rollup (api_key_id, bucket_start);
CREATE INDEX usage_rollup_org ON usage_rollup (organization_id, bucket_start);

CREATE TABLE usage_rollup_hour_new (
    bucket_start       INTEGER NOT NULL,
    api_key_id         TEXT NOT NULL DEFAULT '',
    team_id            TEXT NOT NULL DEFAULT '',
    organization_id    TEXT NOT NULL DEFAULT '',
    model_group        TEXT NOT NULL DEFAULT '',
    deployment_id      TEXT NOT NULL DEFAULT '',
    error_class        TEXT NOT NULL DEFAULT '',
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
    PRIMARY KEY (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id,
                 error_class)
);
INSERT INTO usage_rollup_hour_new (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id, requests, errors, cache_hits, input_tokens, output_tokens, cached_tokens, cache_write_tokens, reasoning_tokens, cost, latency_ms_sum, estimated, aborted, ttft_ms_sum, ttft_count, lat_le_25, lat_le_50, lat_le_100, lat_le_250, lat_le_500, lat_le_1000, lat_le_2500, lat_le_5000, lat_le_10000, lat_le_30000, lat_le_60000, lat_inf, ttft_le_25, ttft_le_50, ttft_le_100, ttft_le_250, ttft_le_500, ttft_le_1000, ttft_le_2500, ttft_le_5000, ttft_le_10000, ttft_le_30000, ttft_le_60000, ttft_inf)
    SELECT bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id, requests, errors, cache_hits, input_tokens, output_tokens, cached_tokens, cache_write_tokens, reasoning_tokens, cost, latency_ms_sum, estimated, aborted, ttft_ms_sum, ttft_count, lat_le_25, lat_le_50, lat_le_100, lat_le_250, lat_le_500, lat_le_1000, lat_le_2500, lat_le_5000, lat_le_10000, lat_le_30000, lat_le_60000, lat_inf, ttft_le_25, ttft_le_50, ttft_le_100, ttft_le_250, ttft_le_500, ttft_le_1000, ttft_le_2500, ttft_le_5000, ttft_le_10000, ttft_le_30000, ttft_le_60000, ttft_inf FROM usage_rollup_hour;
DROP TABLE usage_rollup_hour;
ALTER TABLE usage_rollup_hour_new RENAME TO usage_rollup_hour;
CREATE INDEX usage_rollup_hour_team ON usage_rollup_hour (team_id, bucket_start);
CREATE INDEX usage_rollup_hour_key ON usage_rollup_hour (api_key_id, bucket_start);
CREATE INDEX usage_rollup_hour_org ON usage_rollup_hour (organization_id, bucket_start);
CREATE INDEX usage_rollup_hour_model ON usage_rollup_hour (model_group, bucket_start);
CREATE INDEX usage_rollup_hour_deployment ON usage_rollup_hour (deployment_id, bucket_start);
