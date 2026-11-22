// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// Screenshots der Musterseiten (hell/dunkel × Desktop/schmal) für docs/konzept/ux/.
import { clean, expect, goto, OUT, test } from './fixtures.js';

test.use({ reducedMotion: 'reduce' });

const VIEWPORTS = {
  desktop: { width: 1440, height: 900 },
  schmal: { width: 390, height: 844 },
};

const SCENES = [
  { name: 'uebersicht', path: '/', full: true, vp: ['desktop', 'schmal'] },
  { name: 'keys', path: '/keys', full: true, vp: ['desktop', 'schmal'] },
  {
    name: 'key-erstellen',
    path: '/keys',
    vp: ['desktop'],
    act: async (page) => {
      await page.getByTestId('create-key').click();
      await page.getByRole('textbox', { name: 'Name', exact: true }).fill('support-bot-eu');
      const dlg = page.getByTestId('key-form-dialog');
      await dlg.getByText('Ausgewählte', { exact: true }).click();
      await dlg.getByText('gpt-4o-mini', { exact: true }).click();
    },
  },
  {
    name: 'key-anzeige',
    path: '/keys',
    vp: ['desktop'],
    act: async (page) => {
      await page.getByTestId('create-key').click();
      await page.getByRole('textbox', { name: 'Name', exact: true }).fill('support-bot-eu');
      await page.getByTestId('key-form-submit').click();
      await expect(page.getByTestId('secret-value')).toBeVisible();
    },
  },
  { name: 'modelle', path: '/models', full: true, vp: ['desktop', 'schmal'] },
  { name: 'logs', path: '/logs', vp: ['desktop', 'schmal'] },
  {
    name: 'log-detail',
    path: '/logs',
    vp: ['desktop', 'schmal'],
    act: async (page) => {
      await page.getByTestId('logs-table').locator('tbody tr').nth(2).click();
      await expect(page.getByTestId('log-detail')).toBeVisible();
    },
  },
  {
    name: 'befehlspalette',
    path: '/',
    vp: ['desktop'],
    act: async (page) => {
      await page.getByTestId('search-button').click();
      await expect(page.getByTestId('command-palette')).toBeVisible();
    },
  },
  {
    name: 'navigation',
    path: '/',
    vp: ['schmal'],
    act: async (page) => {
      await page.getByTestId('menu-button').click();
      await expect(page.getByRole('link', { name: 'Keys', exact: true })).toBeVisible();
    },
  },
  { name: 'design-system', path: '/design', full: true, vp: ['desktop'] },
];

for (const theme of ['hell', 'dunkel']) {
  for (const s of SCENES) {
    for (const vp of s.vp) {
      test(`${s.name} ${theme} ${vp}`, async ({ page }) => {
        await page.setViewportSize(VIEWPORTS[vp]);
        await goto(page, s.path, { theme: theme === 'hell' ? 'light' : 'dark' });
        await page.waitForTimeout(300);
        if (s.act) await s.act(page);
        await page.waitForTimeout(400);
        await page.screenshot({ path: `${OUT}/screens/${s.name}-${theme}-${vp}.png`, fullPage: !!s.full && !s.act });
        clean(page);
      });
    }
  }
}

