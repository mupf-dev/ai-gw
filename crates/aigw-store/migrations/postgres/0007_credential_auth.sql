-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-84 Anmeldeart an Zugangsdaten (Postgres; Gegenstück in
-- migrations/sqlite/0007_credential_auth.sql):
--   * provider_credential.auth: nicht geheime Anmeldeangaben als JSON, z. B.
--     {"entra": {"credential": "managedIdentity", "clientId": "…"}}; '{}' = nur Felder in data.
ALTER TABLE provider_credential ADD COLUMN auth JSONB NOT NULL DEFAULT '{}';
