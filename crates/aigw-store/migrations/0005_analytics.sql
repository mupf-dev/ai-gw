-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-46: Dashboards und Log-Explorer.
-- Portabel für SQLite und Postgres; je ALTER genau eine Spalte.

-- Upstream-Versuche (Retries, Fallbacks) als JSON-Array; NULL = keine Versuche aufgezeichnet.
ALTER TABLE request_log ADD COLUMN attempts TEXT;

-- Request-ID des Clients (x-request-id / x-client-request-id), filterbar im Log-Explorer.
ALTER TABLE request_log ADD COLUMN client_request_id TEXT;
CREATE INDEX request_log_client_request_id ON request_log (client_request_id);

-- Indizes für Filter des Log-Explorers (sortiert nach Zeit absteigend).
CREATE INDEX request_log_org_at ON request_log (organization_id, at);
CREATE INDEX request_log_model_at ON request_log (model_group, at);
CREATE INDEX request_log_deployment_at ON request_log (deployment_id, at);

-- TTFT-Histogramm (gleiche Grenzen wie die Latenz, nur Anfragen mit TTFT), für ttft_p95.

ALTER TABLE usage_rollup ADD COLUMN ttft_le_25 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_le_50 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_le_100 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_le_250 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_le_500 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_le_1000 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_le_2500 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_le_5000 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_le_10000 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_le_30000 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_le_60000 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_inf BIGINT NOT NULL DEFAULT 0;

-- Rollup-Filter je Key bzw. Organisation.
CREATE INDEX usage_rollup_key ON usage_rollup (api_key_id, bucket_start);
CREATE INDEX usage_rollup_org ON usage_rollup (organization_id, bucket_start);

-- Stunden-Rollups: gleiche Spalten, Schlüssel mit Stundenbeginn. Der Writer schreibt sie im
-- selben Batch additiv mit; Dashboards über Tage lesen damit ~60× weniger Zeilen.
CREATE TABLE usage_rollup_hour (
    bucket_start     BIGINT NOT NULL,       -- Stundenbeginn (Unix-ms, UTC)
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
    cache_write_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens BIGINT NOT NULL DEFAULT 0,
    cost             BIGINT NOT NULL DEFAULT 0,
    latency_ms_sum   BIGINT NOT NULL DEFAULT 0,
    estimated        BIGINT NOT NULL DEFAULT 0,
    aborted          BIGINT NOT NULL DEFAULT 0,
    ttft_ms_sum      BIGINT NOT NULL DEFAULT 0,
    ttft_count       BIGINT NOT NULL DEFAULT 0,
    lat_le_25        BIGINT NOT NULL DEFAULT 0,
    lat_le_50        BIGINT NOT NULL DEFAULT 0,
    lat_le_100       BIGINT NOT NULL DEFAULT 0,
    lat_le_250       BIGINT NOT NULL DEFAULT 0,
    lat_le_500       BIGINT NOT NULL DEFAULT 0,
    lat_le_1000      BIGINT NOT NULL DEFAULT 0,
    lat_le_2500      BIGINT NOT NULL DEFAULT 0,
    lat_le_5000      BIGINT NOT NULL DEFAULT 0,
    lat_le_10000     BIGINT NOT NULL DEFAULT 0,
    lat_le_30000     BIGINT NOT NULL DEFAULT 0,
    lat_le_60000     BIGINT NOT NULL DEFAULT 0,
    lat_inf          BIGINT NOT NULL DEFAULT 0,
    ttft_le_25       BIGINT NOT NULL DEFAULT 0,
    ttft_le_50       BIGINT NOT NULL DEFAULT 0,
    ttft_le_100      BIGINT NOT NULL DEFAULT 0,
    ttft_le_250      BIGINT NOT NULL DEFAULT 0,
    ttft_le_500      BIGINT NOT NULL DEFAULT 0,
    ttft_le_1000     BIGINT NOT NULL DEFAULT 0,
    ttft_le_2500     BIGINT NOT NULL DEFAULT 0,
    ttft_le_5000     BIGINT NOT NULL DEFAULT 0,
    ttft_le_10000    BIGINT NOT NULL DEFAULT 0,
    ttft_le_30000    BIGINT NOT NULL DEFAULT 0,
    ttft_le_60000    BIGINT NOT NULL DEFAULT 0,
    ttft_inf         BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id)
);
CREATE INDEX usage_rollup_hour_team ON usage_rollup_hour (team_id, bucket_start);
CREATE INDEX usage_rollup_hour_key ON usage_rollup_hour (api_key_id, bucket_start);
CREATE INDEX usage_rollup_hour_org ON usage_rollup_hour (organization_id, bucket_start);
CREATE INDEX usage_rollup_hour_model ON usage_rollup_hour (model_group, bucket_start);
CREATE INDEX usage_rollup_hour_deployment ON usage_rollup_hour (deployment_id, bucket_start);

-- Bestehende Minuten-Rollups übernehmen.
INSERT INTO usage_rollup_hour (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id,
    requests, errors, cache_hits, input_tokens, output_tokens, cached_tokens, cache_write_tokens, reasoning_tokens, cost, latency_ms_sum, estimated, aborted, ttft_ms_sum, ttft_count, lat_le_25, lat_le_50, lat_le_100, lat_le_250, lat_le_500, lat_le_1000, lat_le_2500, lat_le_5000, lat_le_10000, lat_le_30000, lat_le_60000, lat_inf, ttft_le_25, ttft_le_50, ttft_le_100, ttft_le_250, ttft_le_500, ttft_le_1000, ttft_le_2500, ttft_le_5000, ttft_le_10000, ttft_le_30000, ttft_le_60000, ttft_inf)
SELECT (bucket_start / 3600000) * 3600000, api_key_id, team_id, organization_id, model_group, deployment_id,
    SUM(requests), SUM(errors), SUM(cache_hits), SUM(input_tokens), SUM(output_tokens), SUM(cached_tokens), SUM(cache_write_tokens), SUM(reasoning_tokens), SUM(cost), SUM(latency_ms_sum), SUM(estimated), SUM(aborted), SUM(ttft_ms_sum), SUM(ttft_count), SUM(lat_le_25), SUM(lat_le_50), SUM(lat_le_100), SUM(lat_le_250), SUM(lat_le_500), SUM(lat_le_1000), SUM(lat_le_2500), SUM(lat_le_5000), SUM(lat_le_10000), SUM(lat_le_30000), SUM(lat_le_60000), SUM(lat_inf), SUM(ttft_le_25), SUM(ttft_le_50), SUM(ttft_le_100), SUM(ttft_le_250), SUM(ttft_le_500), SUM(ttft_le_1000), SUM(ttft_le_2500), SUM(ttft_le_5000), SUM(ttft_le_10000), SUM(ttft_le_30000), SUM(ttft_le_60000), SUM(ttft_inf)
FROM usage_rollup
GROUP BY (bucket_start / 3600000) * 3600000, api_key_id, team_id, organization_id, model_group, deployment_id;
