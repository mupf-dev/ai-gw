// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
import { defineConfig } from '@playwright/test';

// Läuft gegen ein echtes `aigw serve` (UI eingebettet, CSP aus crates/aigw-admin), gestartet von run.sh.
const base = process.env.AIGW_URL || 'http://127.0.0.1:4899';
const out = process.env.E2E_OUT || './results';

export default defineConfig({
  testDir: './tests',
  workers: 1,
  fullyParallel: false,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  outputDir: `${out}/artifacts`,
  reporter: [['list'], ['json', { outputFile: `${out}/playwright.json` }]],
  use: {
    baseURL: base,
    headless: true,
    browserName: 'chromium',
    locale: 'de-DE',
    timezoneId: 'Europe/Berlin',
    viewport: { width: 1440, height: 900 },
  },
});
