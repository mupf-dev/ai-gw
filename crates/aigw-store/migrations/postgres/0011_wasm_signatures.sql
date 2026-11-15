-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-101 Signaturen der WASM-Module (postgres; Gegenstück in migrations/sqlite/0011_wasm_signatures.sql):
--   Ed25519-Signatur (Base64) über "aigw-wasm-module-v1:<sha256>" und die Schlüssel-ID aus
--   wasm.trusted_keys. Beide NULL = unsigniert.
ALTER TABLE wasm_module ADD COLUMN signature TEXT;
ALTER TABLE wasm_module ADD COLUMN signature_key_id TEXT;
