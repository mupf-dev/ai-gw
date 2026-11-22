-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-100 / S-27 (sqlite; Gegenstück in migrations/postgres/0010_cache_aad.sql):
-- Die Verschlüsselung der Warmstart-Einträge des semantischen Caches bindet jetzt auch die
-- Partition (Mandant/Scope) in die AAD. Einträge mit der alten AAD (nur ID) ließen sich nicht
-- mehr öffnen – es ist ein Cache, daher werden sie verworfen statt umgeschlüsselt.
DELETE FROM cache_vector;
