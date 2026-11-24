-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-126 Budget je Deployment (postgres; Gegenstück in
-- migrations/sqlite/0015_deployment_budget.sql): optionales Budget (JSONB, wie
-- api_key/team/organization.budget). Der Verbrauch läuft über budget_usage mit
-- level = 'deployment' und target_id = deployment.id (keine Schemaänderung dort nötig).
ALTER TABLE deployment ADD COLUMN budget JSONB;
