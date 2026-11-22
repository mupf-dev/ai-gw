-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-117 Kostenanalyse (sqlite; Gegenstück in migrations/postgres/0013_cost_breakdown.sql):
-- Zählspalten der Minuten- und Stunden-Rollups für die Aufschlüsselung der Kosten (Nano-Einheiten):
--   cost_input/cost_cache_read/cost_cache_write/cost_output: Kosten der Hauptanfrage je Token-Art,
--   cache_read_saved: Ersparnis durch Prompt-Caching des Anbieters (Cache-Tokens × (Eingabe − Cache-Lesepreis)),
--   plugin_cost/plugin_input_tokens/plugin_output_tokens: Modellaufrufe von Plugins (content-safety,
--   schema-guard-Wiederholungen; Kosten in cost enthalten, Tokens nicht in input/output_tokens).
-- Rein additiv wie alle Rollup-Spalten; ältere Zeilen haben 0 (= nicht aufgeschlüsselt).
ALTER TABLE usage_rollup ADD COLUMN cost_input INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN cost_cache_read INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN cost_cache_write INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN cost_output INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN cache_read_saved INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN plugin_cost INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN plugin_input_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup ADD COLUMN plugin_output_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup_hour ADD COLUMN cost_input INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup_hour ADD COLUMN cost_cache_read INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup_hour ADD COLUMN cost_cache_write INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup_hour ADD COLUMN cost_output INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup_hour ADD COLUMN cache_read_saved INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup_hour ADD COLUMN plugin_cost INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup_hour ADD COLUMN plugin_input_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollup_hour ADD COLUMN plugin_output_tokens INTEGER NOT NULL DEFAULT 0;
