-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- Inkrementeller Snapshot-Neubau (F16, AIGW-75): Das audit_log dient als Änderungsjournal.
-- Der Reloader liest alle Einträge mit config_version im Bereich (aktiv, neu] – dafür dieser
-- Index (gleich in beiden Migrationssätzen).
CREATE INDEX audit_log_config_version ON audit_log (config_version);
