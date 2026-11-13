-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-43: Request-Log gepuffert/gebündelt, Payload getrennt befristet, additive Rollups.
-- Portabel für SQLite (≥ 3.35, DROP COLUMN) und Postgres; je ALTER genau eine Spalte.

-- Rohlog: fehlende Kennzahlen.
ALTER TABLE request_log ADD COLUMN cache_write_tokens BIGINT NOT NULL DEFAULT 0;
ALTER TABLE request_log ADD COLUMN client_aborted BIGINT NOT NULL DEFAULT 0;

-- Payload in eigener Tabelle: eigene Frist ohne Scan über Rohlogs, schmale request_log-Zeilen.
ALTER TABLE request_log DROP COLUMN payload;
CREATE TABLE request_payload (
    id      TEXT PRIMARY KEY,               -- = request_log.id (kein FK: unabhängig löschbar)
    at      BIGINT NOT NULL,                -- = request_log.at
    mode    TEXT NOT NULL,                  -- metadata | masked | full
    payload TEXT NOT NULL                   -- JSON
);
CREATE INDEX request_payload_at ON request_payload (at);

-- Rollups: Histogramm als feste Spalten statt JSON, damit UPSERT rein additiv ist
-- (beide Datenbanken, mehrere Instanzen ohne Read-Modify-Write). Zählung je Bucket, nicht
-- kumulativ; Obergrenzen in Millisekunden (aigw_store::observe::LATENCY_BUCKETS_MS).
ALTER TABLE usage_rollup DROP COLUMN latency_hist;
ALTER TABLE usage_rollup ADD COLUMN cache_write_tokens BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN estimated BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN aborted BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_ms_sum BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN ttft_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_le_25 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_le_50 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_le_100 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_le_250 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_le_500 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_le_1000 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_le_2500 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_le_5000 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_le_10000 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_le_30000 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_le_60000 BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN lat_inf BIGINT NOT NULL DEFAULT 0;
CREATE INDEX usage_rollup_deployment ON usage_rollup (deployment_id, bucket_start);
