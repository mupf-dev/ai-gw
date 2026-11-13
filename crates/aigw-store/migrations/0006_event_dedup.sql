-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-61: Deduplizierung von Ereignis-Webhooks im Cluster. Wer eine Zeile für einen
-- Schlüssel (z. B. budget:team:<id>:month:<beginn>:warning-80) zuerst anlegt, sendet das
-- Ereignis; alle anderen Instanzen verwerfen es. Abgelaufene Zeilen (expires_at, ms seit
-- Epoche) werden beim nächsten Anspruch gelöscht.

CREATE TABLE event_dedup (
    dedup_key  TEXT PRIMARY KEY,
    event_type TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL
);

CREATE INDEX event_dedup_expires ON event_dedup (expires_at);
