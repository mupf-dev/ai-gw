-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-106 Sigstore-Bundles und Vier-Augen-Freigabe der WASM-Module (sqlite; Gegenstück in
-- migrations/postgres/0014_wasm_approval.sql):
--   * wasm_module: Sigstore-Bundle (JSON), Hochlader der aktiven Version, Freigabe (wer, wann).
--   * wasm_module_pending: hochgeladene, noch nicht freigegebene Version je Modulname (höchstens
--     eine). Nicht Teil des Snapshots – erst die Freigabe schreibt nach wasm_module (Audit-Art
--     `wasm-module`, Journal). uploader_person: die Person hinter dem Hochlader (Admin-Keys →
--     anlegender Nutzer), zum Zeitpunkt des Uploads festgehalten.
ALTER TABLE wasm_module ADD COLUMN sigstore_bundle TEXT;
ALTER TABLE wasm_module ADD COLUMN uploaded_by TEXT;
ALTER TABLE wasm_module ADD COLUMN approved_by TEXT;
ALTER TABLE wasm_module ADD COLUMN approved_at INTEGER;

CREATE TABLE wasm_module_pending (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL UNIQUE,
    sha256           TEXT NOT NULL,
    size             INTEGER NOT NULL,
    description      TEXT,
    content          BLOB NOT NULL,
    signature        TEXT,
    signature_key_id TEXT,
    sigstore_bundle  TEXT,
    uploaded_by      TEXT NOT NULL,
    uploader_person  TEXT NOT NULL,
    uploaded_at      INTEGER NOT NULL
);
