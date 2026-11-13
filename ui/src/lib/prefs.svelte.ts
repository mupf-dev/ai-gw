// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Darstellungs- und Sitzungseinstellungen: Farbschema (System/Hell/Dunkel), Akzentfarbe (Vergleich bis zur
// Entscheidung) und Mock-Modus. Am <html> stehen immer die wirksamen Werte (data-theme, data-accent), damit
// Tokens (light-dark) und die dark:-Variante ohne Inline-Skript (CSP) stimmen.
// Vorbelegung per URL für Demo-Links und Screenshots: ?theme=dark&accent=petrol&mock=0

export type Theme = 'system' | 'light' | 'dark';
export type Accent = 'violett' | 'petrol' | 'bernstein';
export const THEMES: { value: Theme; label: string }[] = [
  { value: 'system', label: 'System' },
  { value: 'light', label: 'Hell' },
  { value: 'dark', label: 'Dunkel' },
];
export const ACCENTS: { value: Accent; label: string; note: string }[] = [
  { value: 'violett', label: 'Violett', note: 'Vorschlag für ai-gw' },
  { value: 'petrol', label: 'Petrol', note: 'wie cloud-project' },
  { value: 'bernstein', label: 'Bernstein', note: 'warm, nah an Warnfarben' },
];

/** Solange die Admin-API nur /version kennt, startet die UI im Mock-Modus. */
const MOCK_DEFAULT = true;

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
  accent: pick(
    ACCENTS.map((a) => a.value),
    q.get('accent'),
    read('aigw.accent'),
  ) ?? ('violett' as Accent),
  mock: mockParam !== null ? mockParam !== '0' : (read('aigw.mock') ?? String(MOCK_DEFAULT)) === 'true',
});

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
  root.dataset.accent = prefs.accent;
  write('aigw.theme', prefs.theme);
  write('aigw.accent', prefs.accent);
  write('aigw.mock', String(prefs.mock));
}

export function setTheme(t: Theme) {
  prefs.theme = t;
  applyPrefs();
}
export function setAccent(a: Accent) {
  prefs.accent = a;
  applyPrefs();
}
export function setMock(on: boolean) {
  prefs.mock = on;
  applyPrefs();
}
