-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-76 Zähler & Beobachtung (Postgres; Gegenstück in migrations/sqlite/0004_observe.sql):
--   * F30/F185: payload_mode und expose_reasoning an Organisation, Team und API-Key (Vererbung
--     Key → Team → Organisation → observe.payload bzw. settings.exposeReasoning),
--   * F47: request_log.error_class (feine Fehlerklasse) und error_class als weitere
--     Schlüsselspalte der Minuten-/Stunden-Rollups (Dashboard-Dimension; '' = kein Fehler),
--   * F100: request_log.trace_id (Verknüpfung Log ↔ Trace),
--   * F161: request_log.guardrails (Zähler der Guardrail-Erkennungen als JSON, ohne Inhalte).

-- F30/F185: Vererbbare Einstellungen (NULL = nicht gesetzt, erbt).
ALTER TABLE organization ADD COLUMN payload_mode TEXT, ADD COLUMN expose_reasoning BOOLEAN;
ALTER TABLE team ADD COLUMN payload_mode TEXT, ADD COLUMN expose_reasoning BOOLEAN;
ALTER TABLE api_key ADD COLUMN payload_mode TEXT, ADD COLUMN expose_reasoning BOOLEAN;

-- F47/F100/F161: Rohlog.
ALTER TABLE request_log
    ADD COLUMN error_class TEXT,
    ADD COLUMN trace_id    TEXT,
    ADD COLUMN guardrails  JSONB;
CREATE INDEX request_log_error_class_at ON request_log (error_class, at)
    WHERE error_class IS NOT NULL;
CREATE INDEX request_log_trace ON request_log (trace_id) WHERE trace_id IS NOT NULL;

-- F47: error_class als Schlüsselspalte der Rollups (bestehende Zeilen: '').
ALTER TABLE usage_rollup ADD COLUMN error_class TEXT NOT NULL DEFAULT '';
ALTER TABLE usage_rollup DROP CONSTRAINT usage_rollup_pkey;
ALTER TABLE usage_rollup ADD PRIMARY KEY
    (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id, error_class);
ALTER TABLE usage_rollup_hour ADD COLUMN error_class TEXT NOT NULL DEFAULT '';
ALTER TABLE usage_rollup_hour DROP CONSTRAINT usage_rollup_hour_pkey;
ALTER TABLE usage_rollup_hour ADD PRIMARY KEY
    (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id, error_class);
