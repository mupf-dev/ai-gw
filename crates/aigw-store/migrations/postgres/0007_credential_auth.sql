-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-84 Anmeldeart an Zugangsdaten (Postgres; Gegenstück in
-- migrations/sqlite/0007_credential_auth.sql):
--   * provider_credential.auth: nicht geheime Anmeldeangaben als JSON, z. B.
--     {"entra": {"credential": "managedIdentity", "clientId": "…"}}; '{}' = nur Felder in data.
ALTER TABLE provider_credential ADD COLUMN auth JSONB NOT NULL DEFAULT '{}';
