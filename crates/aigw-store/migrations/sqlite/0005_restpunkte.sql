-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-78 „Restpunkte“ (SQLite). Gegenstück: migrations/postgres/0005_restpunkte.sql.

-- F117: Kettensteuerung der Plugin-Bindungen als eigene Spalten. Bestehende Bindungen mit
-- diesen Schlüsseln in `config` werden beim Lesen übernommen (und beim nächsten Schreiben
-- umgeschrieben).
ALTER TABLE plugin_binding ADD COLUMN inherit TEXT;
ALTER TABLE plugin_binding ADD COLUMN locked INTEGER NOT NULL DEFAULT 0;
ALTER TABLE plugin_binding ADD COLUMN fail_mode TEXT;
ALTER TABLE plugin_binding ADD COLUMN timeout_ms INTEGER;
ALTER TABLE plugin_binding ADD COLUMN output_reject_mode TEXT;

-- F115: Arten im Audit-Log einheitlich kebab-case (wie `admin-session`).
UPDATE audit_log SET kind = CASE kind
    WHEN 'Organization' THEN 'organization'
    WHEN 'Team' THEN 'team'
    WHEN 'User' THEN 'user'
    WHEN 'ApiKey' THEN 'api-key'
    WHEN 'ProviderCredential' THEN 'provider-credential'
    WHEN 'ModelGroup' THEN 'model-group'
    WHEN 'Deployment' THEN 'deployment'
    WHEN 'Price' THEN 'price'
    WHEN 'PluginBinding' THEN 'plugin-binding'
    WHEN 'McpServer' THEN 'mcp-server'
    WHEN 'McpGrant' THEN 'mcp-grant'
    ELSE kind END;

-- F116: Hash-Kette. Bestehende Einträge bleiben ohne Kettenfelder („unverkettet“); die Kette
-- beginnt mit dem ersten Eintrag nach dieser Migration. `audit_chain` hält den Kopf (letzte
-- Nummer und letzter Hash) und den Anker nach Löschungen durch die Aufbewahrung.
ALTER TABLE audit_log ADD COLUMN seq INTEGER;
ALTER TABLE audit_log ADD COLUMN prev_hash TEXT;
ALTER TABLE audit_log ADD COLUMN hash TEXT;
CREATE UNIQUE INDEX audit_log_seq ON audit_log (seq);
CREATE TABLE audit_chain (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    seq         INTEGER NOT NULL,
    head        TEXT NOT NULL,
    anchor_seq  INTEGER NOT NULL,
    anchor_hash TEXT NOT NULL
);
INSERT INTO audit_chain (id, seq, head, anchor_seq, anchor_hash) VALUES (
    1, 0,
    '0000000000000000000000000000000000000000000000000000000000000000',
    0,
    '0000000000000000000000000000000000000000000000000000000000000000'
);
