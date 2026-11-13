-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-79 Payload-Richtlinie (Postgres; Gegenstück in migrations/sqlite/0006_payload_policy.sql):
--   * S-09/F203: payload_mode_max (Obergrenze off < metadata < masked < full, NULL = keine)
--     und payload_mode_locked (untergeordnete Ebenen dürfen payloadMode nicht setzen) an
--     Organisation und Team.
ALTER TABLE organization ADD COLUMN payload_mode_max TEXT,
    ADD COLUMN payload_mode_locked BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE team ADD COLUMN payload_mode_max TEXT,
    ADD COLUMN payload_mode_locked BOOLEAN NOT NULL DEFAULT FALSE;
