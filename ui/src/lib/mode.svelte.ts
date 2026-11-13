// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Wirksame Datenquelle: Mock-Modus an (Schalter bzw. ?mock=1) → Beispieldaten aus dem Mock-Backend, sonst die
// echte Admin-API (Standard; User-Entscheidung D6). Mit `admin.ui.mock: false` (F135) gibt es keinen Mock-Modus.
import { setMockResolver } from './api/client';
import { prefs } from './prefs.svelte';
import { auth } from './session.svelte';

export function isMock(): boolean {
  return prefs.mockMode === 'on' && auth.uiMock;
}

/** Mock-Modus überhaupt angeboten (`admin.ui.mock`, F135)? Sonst sind alle Schalter ausgeblendet. */
export function mockAvailable(): boolean {
  return auth.uiMock;
}

/** Schlüssel der Datenquelle: Seiten werden bei einem Wechsel neu aufgebaut (Mock ↔ echt, anderer Principal). */
export function dataKey(): string {
  return isMock() ? 'mock' : `real:${auth.session?.subject ?? '-'}`;
}

setMockResolver(isMock);
