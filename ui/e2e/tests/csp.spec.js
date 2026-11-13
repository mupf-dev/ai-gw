// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-48: Bits-UI-Overlays (Popover/Dropdown/Select, Dialog, Sheet, Tooltip) und das Diagramm (uPlot)
// funktionieren unter der strikten CSP von aigw-admin – ohne Verstöße und ohne Konsolenfehler.
import { clean, expect, goto, test } from './fixtures.js';

test('CSP-Header wird ausgeliefert', async ({ page }) => {
  const res = await page.goto('/admin/');
  const csp = res.headers()['content-security-policy'];
  expect(csp).toContain("style-src 'self'");
  expect(csp).toContain("script-src 'self'");
  expect(csp).not.toContain('unsafe-inline');
});

test('Übersicht: Diagramm, Tabs, Zeitraum, Tooltip', async ({ page }) => {
  await goto(page, '/');
  const chart = page.locator('.gw-chart canvas');
  await expect(chart).toBeVisible();
  const over = page.locator('.gw-chart .u-over');
  const box = await over.boundingBox();
  await page.mouse.move(box.x + box.width * 0.6, box.y + box.height / 2);
  await expect(page.locator('figure [aria-hidden="true"].shadow-pop')).toBeVisible();
  await page.getByRole('tab', { name: 'p95-Latenz' }).click();
  await page.getByRole('radio', { name: '7 Tage' }).click();
  await page.getByTestId('mock-badge').hover();
  await expect(page.getByRole('tooltip')).toBeVisible();
  clean(page);
});

test('Topbar: Kontext-Umschalter, Nutzer-Menü, Befehlspalette', async ({ page }) => {
  await goto(page, '/');
  await page.getByTestId('context-switcher').click();
  await expect(page.getByRole('menuitemradio', { name: /Data Science/ })).toBeVisible();
  await page.getByRole('menuitemradio', { name: /Data Science/ }).click();
  await page.getByTestId('user-menu').click();
  await page.getByRole('menuitemradio', { name: 'Dunkel' }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await page.keyboard.press('Escape');
  await page.keyboard.press('Control+k');
  await expect(page.getByTestId('command-palette')).toBeVisible();
  await page.keyboard.type('logs');
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/\/admin\/logs/);
  clean(page);
});

test('Keys: Select-Filter, Dropdown, Dialog „Key erstellen“, einmalige Anzeige', async ({ page }) => {
  await goto(page, '/keys');
  await page.getByRole('combobox', { name: 'Status' }).click();
  await page.getByRole('option', { name: 'Aktiv' }).click();
  await page.getByRole('button', { name: /Aktionen für/ }).first().click();
  await expect(page.getByRole('menuitem', { name: /Rotieren/ })).toBeVisible();
  await page.keyboard.press('Escape');
  await page.getByTestId('create-key').click();
  await page.getByTestId('create-key-submit').click();
  await expect(page.getByText('Bitte einen Namen angeben.')).toBeVisible();
  await page.getByRole('textbox', { name: 'Name', exact: true }).fill('csp-test-key');
  await page.getByTestId('create-key-submit').click();
  await expect(page.getByTestId('secret-value')).toContainText('sk-gw-');
  await page.getByRole('checkbox').click();
  await page.getByTestId('key-done').click();
  await expect(page.getByRole('status').filter({ hasText: 'Key erstellt' })).toBeVisible();
  clean(page);
});

test('Logs: Detail-Sheet mit Tabs und Code-Anzeige', async ({ page }) => {
  await goto(page, '/logs');
  await page.getByTestId('logs-table').locator('tbody tr').first().click();
  const sheet = page.getByTestId('log-detail');
  await expect(sheet).toBeVisible();
  await sheet.getByRole('tab', { name: 'Request' }).click();
  await expect(sheet.getByRole('tabpanel', { name: 'Request' }).locator('pre')).toBeVisible();
  await sheet.getByRole('tab', { name: 'Zeitleiste' }).click();
  await page.keyboard.press('Escape');
  await expect(sheet).toBeHidden();
  clean(page);
});

test('Modelle: Gruppenwechsel, Tabs, Switch, Tooltip', async ({ page }) => {
  await goto(page, '/models');
  await page.getByRole('button', { name: /claude-sonnet/ }).first().click();
  await expect(page.getByTestId('group-detail')).toContainText('claude-sonnet');
  await page.getByRole('tab', { name: /Routing-Regeln/ }).click();
  await page.getByRole('switch').first().click();
  clean(page);
});

test('Design-System: alle Overlays', async ({ page }) => {
  await goto(page, '/design');
  await page.getByTestId('open-dropdown').click();
  await expect(page.getByTestId('dropdown-content')).toBeVisible();
  await page.keyboard.press('Escape');
  await page.getByTestId('tooltip-trigger').hover();
  await expect(page.getByRole('tooltip')).toBeVisible();
  await page.getByTestId('open-dialog').click();
  await expect(page.getByTestId('demo-dialog')).toBeVisible();
  await page.keyboard.press('Escape');
  await page.getByTestId('open-sheet').click();
  await expect(page.getByTestId('demo-sheet')).toBeVisible();
  await page.keyboard.press('Escape');
  await page.getByTestId('open-toast').click();
  await expect(page.getByTestId('toast')).toBeVisible();
  clean(page);
});

test('Mobil: Navigation im Sheet', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await goto(page, '/');
  await page.getByTestId('menu-button').click();
  await page.getByRole('link', { name: 'Keys' }).click();
  await expect(page).toHaveURL(/\/admin\/keys/);
  clean(page);
});

test('Ohne Mock: echte API (Version), Leerzustände', async ({ page }) => {
  await goto(page, '/', { mock: false });
  await expect(page.getByTestId('version')).not.toHaveText('–');
  await goto(page, '/keys', { mock: false });
  await expect(page.getByText('Keys noch nicht verfügbar')).toBeVisible();
  clean(page);
});
