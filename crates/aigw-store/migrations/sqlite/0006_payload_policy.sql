-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-79 Payload-Richtlinie (SQLite; Gegenstück in migrations/postgres/0006_payload_policy.sql):
--   * S-09/F203: payload_mode_max (Obergrenze off < metadata < masked < full, NULL = keine)
--     und payload_mode_locked (untergeordnete Ebenen dürfen payloadMode nicht setzen) an
--     Organisation und Team.
ALTER TABLE organization ADD COLUMN payload_mode_max TEXT;
ALTER TABLE organization ADD COLUMN payload_mode_locked INTEGER NOT NULL DEFAULT 0;
ALTER TABLE team ADD COLUMN payload_mode_max TEXT;
ALTER TABLE team ADD COLUMN payload_mode_locked INTEGER NOT NULL DEFAULT 0;
