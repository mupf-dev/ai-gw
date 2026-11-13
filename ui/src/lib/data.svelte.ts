// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Datenquelle der Seiten, für die es noch keine Admin-API gibt (Übersicht, Logs – Rollups/Logs folgen mit
// AIGW-46/53). Mock-Modus: deterministische Beispieldaten aus mock/data.ts; ohne Mock liefern die Seiten
// `unavailable` und zeigen einen Leerzustand statt erfundener Zahlen.
// Verwaltungsseiten (Keys, Modelle, Teams …) nutzen api/resources.ts – im Mock-Modus über mock/backend.ts.
import { MOCK, overview as mockOverview } from './mock/data';
import type { LogEntry, Overview, RangeId } from './mock/types';
import { isMock } from './mode.svelte';

export type Result<T> = { state: 'ok'; data: T } | { state: 'unavailable'; reason: string };

const UNAVAILABLE = (what: string): Result<never> => ({
  state: 'unavailable',
  reason: `Die Admin-API stellt ${what} noch nicht bereit (folgt mit AIGW-46/53). Mock-Modus in den Einstellungen oder oben rechts einschalten.`,
});

export function overview(range: RangeId): Result<Overview> {
  return isMock() ? { state: 'ok', data: mockOverview(range) } : UNAVAILABLE('Kennzahlen');
}

export function logs(): Result<LogEntry[]> {
  return isMock() ? { state: 'ok', data: MOCK.logs } : UNAVAILABLE('Request-Logs');
}
