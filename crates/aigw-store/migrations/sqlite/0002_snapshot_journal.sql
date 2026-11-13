-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Inkrementeller Snapshot-Neubau (F16, AIGW-75): Das audit_log dient als Änderungsjournal.
-- Der Reloader liest alle Einträge mit config_version im Bereich (aktiv, neu] – dafür dieser
-- Index (gleich in beiden Migrationssätzen).
CREATE INDEX audit_log_config_version ON audit_log (config_version);
