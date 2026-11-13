// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
import { test as base, expect } from '@playwright/test';
import fs from 'node:fs';

export const OUT = process.env.E2E_OUT || './results';

/** Seite mit Sammlung von CSP-Verstößen (securitypolicyviolation) und Konsolenfehlern. */
export const test = base.extend({
  page: async ({ page }, use, testInfo) => {
    const violations = [];
    const errors = [];
    await page.exposeFunction('__cspViolation', (v) => violations.push(v));
    await page.addInitScript(() => {
      document.addEventListener('securitypolicyviolation', (e) =>
        window.__cspViolation({ directive: e.effectiveDirective, blocked: e.blockedURI, sample: e.sample, source: e.sourceFile, line: e.lineNumber }),
      );
    });
    page.on('console', (m) => {
      if (m.type() === 'error' || /Content Security Policy/i.test(m.text())) errors.push(m.text());
    });
    page.on('pageerror', (e) => errors.push(String(e)));
    page.cspViolations = violations;
    page.consoleErrors = errors;
    await use(page);
    fs.mkdirSync(OUT, { recursive: true });
    fs.appendFileSync(`${OUT}/csp.jsonl`, `${JSON.stringify({ test: testInfo.title, violations, errors })}\n`);
  },
});

export { expect };

/** Seite öffnen und warten, bis die Überschrift steht. */
export async function goto(page, path, { theme = 'light', mock = true } = {}) {
  const sep = path.includes('?') ? '&' : '?';
  await page.goto(`/admin${path}${sep}theme=${theme}&mock=${mock ? 1 : 0}`);
  await expect(page.locator('main h1').first()).toBeVisible({ timeout: 15_000 });
}

/**
 * Keine CSP-Verstöße, keine Konsolenfehler. `allow`: erwartete Meldungen (z. B. Chromiums
 * „Failed to load resource … 412“ beim absichtlich provozierten ETag-Konflikt).
 */
export function clean(page, { allow = [] } = {}) {
  expect(page.cspViolations, `CSP-Verstöße: ${JSON.stringify(page.cspViolations)}`).toEqual([]);
  const errors = page.consoleErrors.filter((e) => !allow.some((re) => re.test(e)));
  expect(errors, `Konsolenfehler: ${JSON.stringify(errors)}`).toEqual([]);
}

/** Bootstrap-Token des Testservers (run.sh setzt AIGW_BOOTSTRAP_TOKEN). */
export const TOKEN = process.env.AIGW_TOKEN || '';

/** Admin-API direkt (Bearer = Bootstrap-Token bzw. `token`); liefert { status, body, etag }. */
export async function api(request, method, path, body, token = TOKEN, headers = {}) {
  const res = await request.fetch(`/admin/api/v1/${path}`, {
    method,
    headers: { authorization: `Bearer ${token}`, ...(body !== undefined ? { 'content-type': 'application/json' } : {}), ...headers },
    data: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  let parsed = null;
  try {
    parsed = text ? JSON.parse(text) : null;
  } catch {
    parsed = text;
  }
  return { status: res.status(), body: parsed, etag: res.headers()['etag'] ?? null };
}

/** Anlegen, vorhandene (409) stillschweigend übernehmen – Testdaten sind idempotent. */
export async function ensure(request, path, body) {
  const r = await api(request, 'POST', path, body);
  if (r.status !== 201 && r.status !== 409) throw new Error(`POST ${path}: ${r.status} ${JSON.stringify(r.body)}`);
  return r;
}

/** Token vor dem Laden in den sessionStorage legen (wie nach dem Token-Dialog). */
export async function useToken(page, token = TOKEN) {
  await page.addInitScript((t) => sessionStorage.setItem('aigw.adminToken', t), token);
}

/** Echte Seite (Mock automatisch → angemeldet = echte Daten) öffnen. */
export async function gotoReal(page, path, { theme = 'light' } = {}) {
  const sep = path.includes('?') ? '&' : '?';
  await page.goto(`/admin${path}${sep}theme=${theme}`);
  await expect(page.locator('main h1').first()).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId('mock-badge')).toHaveCount(0);
}
