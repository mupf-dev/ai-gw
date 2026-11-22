-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-63 WASM-Plugins (Postgres; Gegenstück in migrations/sqlite/0009_wasm_modules.sql):
--   * wasm_module: hochgeladene Komponenten (Inhalt + SHA-256). Teil des Snapshots; jede
--     Änderung schreibt einen Audit-Eintrag (Art `wasm-module`) mit object_id (Journal).
CREATE TABLE wasm_module (
    id          UUID PRIMARY KEY,
    name        TEXT COLLATE "C" NOT NULL UNIQUE,
    sha256      TEXT NOT NULL,
    size        BIGINT NOT NULL,
    description TEXT,
    content     BYTEA NOT NULL,
    created_by  TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL
);
