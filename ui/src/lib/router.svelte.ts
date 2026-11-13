// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Minimaler History-Router unter /admin/ (aigw-admin liefert für Pfade ohne Dateiendung index.html).
// Links sind echte <a href>, Klicks werden abgefangen (Strg/Mittelklick öffnen weiter einen neuen Tab).

export const BASE = '/admin';

function current(): string {
  const p = location.pathname.startsWith(BASE) ? location.pathname.slice(BASE.length) : location.pathname;
  return p === '' ? '/' : p;
}

export const route = $state({ path: current(), search: location.search });

export function href(path: string): string {
  return BASE + (path === '/' ? '/' : path);
}

export function navigate(path: string, replace = false) {
  const url = href(path);
  if (url === location.pathname + location.search) return;
  if (replace) history.replaceState({}, '', url);
  else history.pushState({}, '', url);
  sync();
  window.scrollTo({ top: 0 });
}

function sync() {
  route.path = current();
  route.search = location.search;
}

addEventListener('popstate', sync);

/** Klick-Abfang für interne Links (an der App-Wurzel registriert). */
export function interceptLinks(e: MouseEvent) {
  if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
  const a = (e.target as Element).closest('a');
  if (!a || a.target || a.hasAttribute('download')) return;
  const url = new URL(a.href, location.href);
  if (url.origin !== location.origin || !url.pathname.startsWith(BASE)) return;
  e.preventDefault();
  navigate(url.pathname.slice(BASE.length) + url.search);
}
