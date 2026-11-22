-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-115 Workload-/Agent-Identitäten (postgres; Gegenstück in migrations/sqlite/0012_workload_identities.sql):
--   * api_key.workload_identities: JSON-Array [{issuer, subject, description?}] – Identitäten
--     (z. B. Microsoft Entra Agent ID), die sich mit Access-Token statt Klartext-Key anmelden.
--     Eindeutigkeit über alle Keys prüft das Repository beim Schreiben.
--   * request_log.workload: geprüfte Identität der Anfrage (Aussteller, Kennung, Art, Nutzer,
--     Blueprint) als JSON; NULL bei Anmeldung mit API-Key. Nie das Token selbst.
ALTER TABLE api_key ADD COLUMN workload_identities JSONB NOT NULL DEFAULT '[]';
ALTER TABLE request_log ADD COLUMN workload JSONB;
