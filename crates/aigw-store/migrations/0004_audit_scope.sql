-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-66: Audit-Log-Endpunkt. Jeder Eintrag trägt den Geltungsbereich des betroffenen
-- Objekts (Organisation/Team als Namen, zum Zeitpunkt der Änderung), damit org-/team-admins
-- nur ihren Bereich lesen. *_before = Bereich des Zustands vorher (Verschieben eines Teams
-- oder Keys bleibt für den alten Bereich sichtbar). Ereignisse ohne Konfigurationsänderung
-- (Login, Logout, Erstnutzung eines Admin-Keys …) tragen die damals aktuelle config_version.

ALTER TABLE audit_log ADD COLUMN organization TEXT;
ALTER TABLE audit_log ADD COLUMN team TEXT;
ALTER TABLE audit_log ADD COLUMN organization_before TEXT;
ALTER TABLE audit_log ADD COLUMN team_before TEXT;

-- Bestand: Organisationen tragen ihren eigenen Namen. Teams/Keys älterer Einträge bleiben ohne
-- Bereich (nur für globale Leser sichtbar) – die Ableitung bräuchte JSON-Funktionen, die SQLite
-- und Postgres unterschiedlich schreiben.
UPDATE audit_log SET organization = object_name WHERE kind = 'Organization';
UPDATE audit_log SET organization_before = object_name
    WHERE kind = 'Organization' AND before_json IS NOT NULL;

-- Keyset-Paginierung (neueste zuerst) und Filter.
DROP INDEX audit_log_at;
CREATE INDEX audit_log_at_id ON audit_log (at, id);
CREATE INDEX audit_log_actor ON audit_log (actor, at);
CREATE INDEX audit_log_name ON audit_log (object_name, at);
CREATE INDEX audit_log_org ON audit_log (organization, at);
CREATE INDEX audit_log_team ON audit_log (team, at);
