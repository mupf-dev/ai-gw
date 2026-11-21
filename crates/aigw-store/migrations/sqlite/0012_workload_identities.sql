-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-115 Workload-/Agent-Identitäten (sqlite; Gegenstück in migrations/postgres/0012_workload_identities.sql):
--   * api_key.workload_identities: JSON-Array [{issuer, subject, description?}] – Identitäten
--     (z. B. Microsoft Entra Agent ID), die sich mit Access-Token statt Klartext-Key anmelden.
--     Eindeutigkeit über alle Keys prüft das Repository beim Schreiben.
--   * request_log.workload: geprüfte Identität der Anfrage (Aussteller, Kennung, Art, Nutzer,
--     Blueprint) als JSON; NULL bei Anmeldung mit API-Key. Nie das Token selbst.
ALTER TABLE api_key ADD COLUMN workload_identities TEXT NOT NULL DEFAULT '[]';
ALTER TABLE request_log ADD COLUMN workload TEXT;
