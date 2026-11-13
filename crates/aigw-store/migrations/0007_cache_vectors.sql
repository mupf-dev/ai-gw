-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-40: Warmstart des semantischen Caches (Backend `memory`, eigener HNSW im RAM).
-- Vektor und gecachte Antwort liegen gemeinsam verschlüsselt (AES-256-GCM mit dem Master-Key,
-- AAD = id) – Antworten und Embeddings können personenbezogene Daten enthalten.
-- Portabel für SQLite und Postgres (das Backend `pgvector` legt seine Tabelle selbst an,
-- weil es die Erweiterung `vector` braucht).
CREATE TABLE cache_vector (
    id            TEXT PRIMARY KEY,         -- BLAKE3 (hex) des exakten Cache-Schlüssels
    partition_key TEXT NOT NULL,            -- BLAKE3 (hex) aus Scope, Modellgruppe, Systemprompt/Tools …
    dims          BIGINT NOT NULL,          -- Länge des Vektors
    created_at    BIGINT NOT NULL,          -- Unix-Millisekunden
    expires_at    BIGINT NOT NULL,          -- Unix-Millisekunden
    key_id        TEXT NOT NULL,            -- Master-Key-ID
    nonce         TEXT NOT NULL,            -- hex
    ciphertext    TEXT NOT NULL             -- hex: f32-LE-Vektor ‖ Antwort-JSON, inkl. Tag
);
CREATE INDEX cache_vector_expires ON cache_vector (expires_at);
