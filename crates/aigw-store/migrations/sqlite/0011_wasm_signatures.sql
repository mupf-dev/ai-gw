-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-101 Signaturen der WASM-Module (sqlite; Gegenstück in migrations/postgres/0011_wasm_signatures.sql):
--   Ed25519-Signatur (Base64) über "aigw-wasm-module-v1:<sha256>" und die Schlüssel-ID aus
--   wasm.trusted_keys. Beide NULL = unsigniert.
ALTER TABLE wasm_module ADD COLUMN signature TEXT;
ALTER TABLE wasm_module ADD COLUMN signature_key_id TEXT;
