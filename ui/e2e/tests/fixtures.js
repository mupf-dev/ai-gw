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

export function clean(page) {
  expect(page.cspViolations, `CSP-Verstöße: ${JSON.stringify(page.cspViolations)}`).toEqual([]);
  expect(page.consoleErrors, `Konsolenfehler: ${JSON.stringify(page.consoleErrors)}`).toEqual([]);
}
