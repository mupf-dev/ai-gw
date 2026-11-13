// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Datenquelle der Seiten. Mock-Modus (prefs.mock): deterministische Beispieldaten aus mock/data.ts.
// Ohne Mock: echte Admin-API – solange es dort nur /version gibt, liefern die Seiten `unavailable`
// und zeigen einen Leerzustand mit Hinweis statt erfundener Zahlen.
import { MOCK, MODEL_GROUPS, ROUTES, createKey as mockCreateKey, overview as mockOverview } from './mock/data';
import type { ApiKey, LogEntry, ModelGroup, Overview, RangeId, RouteRule } from './mock/types';
import { prefs } from './prefs.svelte';

export type Result<T> = { state: 'ok'; data: T } | { state: 'unavailable'; reason: string };

const UNAVAILABLE = (what: string): Result<never> => ({
  state: 'unavailable',
  reason: `Die Admin-API stellt ${what} noch nicht bereit. Mock-Modus in den Einstellungen oder oben rechts einschalten.`,
});

const store = $state({ keys: MOCK.keys });

export function overview(range: RangeId): Result<Overview> {
  return prefs.mock ? { state: 'ok', data: mockOverview(range) } : UNAVAILABLE('Kennzahlen');
}

export function keys(): Result<ApiKey[]> {
  return prefs.mock ? { state: 'ok', data: store.keys } : UNAVAILABLE('Keys');
}

export function createKey(input: Parameters<typeof mockCreateKey>[0]): { key: ApiKey; secret: string } {
  const res = mockCreateKey(input);
  store.keys = [res.key, ...store.keys];
  return res;
}

export function revokeKey(id: string) {
  store.keys = store.keys.map((k) => (k.id === id ? { ...k, status: 'revoked' as const } : k));
}

export function models(): Result<{ groups: ModelGroup[]; routes: RouteRule[] }> {
  return prefs.mock ? { state: 'ok', data: { groups: MODEL_GROUPS, routes: ROUTES } } : UNAVAILABLE('Modelle und Routen');
}

export function logs(): Result<LogEntry[]> {
  return prefs.mock ? { state: 'ok', data: MOCK.logs } : UNAVAILABLE('Request-Logs');
}
