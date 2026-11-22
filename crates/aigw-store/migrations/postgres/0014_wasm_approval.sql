-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-106 Sigstore-Bundles und Vier-Augen-Freigabe der WASM-Module (postgres; Gegenstück in
-- migrations/sqlite/0014_wasm_approval.sql):
--   * wasm_module: Sigstore-Bundle (JSON), Hochlader der aktiven Version, Freigabe (wer, wann).
--   * wasm_module_pending: hochgeladene, noch nicht freigegebene Version je Modulname (höchstens
--     eine). Nicht Teil des Snapshots – erst die Freigabe schreibt nach wasm_module (Audit-Art
--     `wasm-module`, Journal). uploader_person: die Person hinter dem Hochlader (Admin-Keys →
--     anlegender Nutzer), zum Zeitpunkt des Uploads festgehalten.
ALTER TABLE wasm_module ADD COLUMN sigstore_bundle TEXT;
ALTER TABLE wasm_module ADD COLUMN uploaded_by TEXT;
ALTER TABLE wasm_module ADD COLUMN approved_by TEXT;
ALTER TABLE wasm_module ADD COLUMN approved_at TIMESTAMPTZ;

CREATE TABLE wasm_module_pending (
    id               UUID PRIMARY KEY,
    name             TEXT COLLATE "C" NOT NULL UNIQUE,
    sha256           TEXT NOT NULL,
    size             BIGINT NOT NULL,
    description      TEXT,
    content          BYTEA NOT NULL,
    signature        TEXT,
    signature_key_id TEXT,
    sigstore_bundle  TEXT,
    uploaded_by      TEXT NOT NULL,
    uploader_person  TEXT NOT NULL,
    uploaded_at      TIMESTAMPTZ NOT NULL
);
