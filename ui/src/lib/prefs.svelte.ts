// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// Darstellungs- und Sitzungseinstellungen: Farbschema (System/Hell/Dunkel) und Mock-Modus (An/Aus, Standard aus). Am <html> stehen immer die wirksamen Werte (data-theme), damit
// Tokens (light-dark) und die dark:-Variante ohne Inline-Skript (CSP) stimmen.
// Vorbelegung per URL für Demo-Links und Screenshots: ?theme=dark&mock=1

export type Theme = 'system' | 'light' | 'dark';
export const THEMES: { value: Theme; label: string }[] = [
  { value: 'system', label: 'System' },
  { value: 'light', label: 'Hell' },
  { value: 'dark', label: 'Dunkel' },
];

/**
 * Mock-Modus (User-Entscheidung D6): standardmäßig **aus**; nur per Schalter (Einstellungen, Nutzer-Menü,
 * Befehlspalette) oder `?mock=1` (gilt nur für diesen Aufruf, nicht gespeichert) – dann deutlich gekennzeichnet.
 */
export type MockMode = 'on' | 'off';

function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
function write(key: string, v: string) {
  try {
    localStorage.setItem(key, v);
  } catch {
    /* Speicher nicht verfügbar (privates Fenster) – Einstellung gilt nur für die Sitzung */
  }
}
function pick<T extends string>(allowed: readonly T[], ...candidates: Array<string | null>): T | undefined {
  return candidates.find((c): c is T => c !== null && (allowed as readonly string[]).includes(c));
}

const q = new URLSearchParams(location.search);
const mockParam = q.get('mock');

export const prefs = $state({
  theme: pick(
    THEMES.map((t) => t.value),
    q.get('theme'),
    read('aigw.theme'),
  ) ?? ('system' as Theme),
  mockMode: mockParam !== null ? (mockParam === '0' ? 'off' : 'on') : (pick(['on', 'off'] as const, read('aigw.mockMode')) ?? 'off'),
});
// URL-Vorgabe gilt nur für diesen Aufruf, die gespeicherte Wahl bleibt unberührt.
let persistMock = mockParam === null;

const media = matchMedia('(prefers-color-scheme: dark)');
export const env = $state({ systemDark: media.matches });
media.addEventListener('change', () => {
  env.systemDark = media.matches;
  applyPrefs();
});

/** Wirksames Schema. */
export function isDark(): boolean {
  return prefs.theme === 'dark' || (prefs.theme === 'system' && env.systemDark);
}

export function applyPrefs() {
  const root = document.documentElement;
  root.dataset.theme = isDark() ? 'dark' : 'light';
  write('aigw.theme', prefs.theme);
  if (persistMock) write('aigw.mockMode', prefs.mockMode);
}

export function setTheme(t: Theme) {
  prefs.theme = t;
  applyPrefs();
}
export function setMockMode(m: MockMode) {
  prefs.mockMode = m;
  persistMock = true;
  applyPrefs();
}
