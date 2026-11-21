// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-117 gegen das echte `aigw serve`: Seite „Kosten“ mit echtem Verkehr (Mock-Deployment mit Preis) – Kennzahlen,
// gestapelter Verlauf mit Tabellen-Alternative, Kostentreiber mit Drilldown (Filter, Log-Explorer), Budget mit Prognose
// und Burn-down, Chargeback-Export (CSV mit Kostenstelle), Befehlspalette, Sichtregeln der Admin-API; Screenshots
// hell/dunkel für docs/konzept/ux/ (echt-kosten-*). Jeweils ohne CSP-Verstöße.
import fs from 'node:fs';
import { api, clean, ensure, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const ORG = 'e117-org';
const ALPHA = 'e117-alpha';
const BETA = 'e117-beta';
const RUN = Date.now().toString(36);
const KEY_A = `e117-a-${RUN}`;
const KEY_B = `e117-b-${RUN}`;

test.beforeAll(async ({ request }) => {
  await ensure(request, 'organizations', { metadata: { name: ORG }, spec: { displayName: 'AIGW-117 Org', metadata: { costCenter: 'CC-ORG' } } });
  await ensure(request, 'teams', {
    metadata: { name: ALPHA, labels: { bereich: 'it' } },
    spec: { organization: ORG, displayName: 'Alpha', metadata: { costCenter: 'CC-ALPHA' }, budget: { amount: '1', period: 'month', warnAtPercent: [50, 80], hardLimit: false } },
  });
  await ensure(request, 'teams', { metadata: { name: BETA }, spec: { organization: ORG, displayName: 'Beta' } });
  await ensure(request, 'model-groups', { metadata: { name: 'e117-echo' }, spec: { routing: 'weighted' } });
  await ensure(request, 'deployments', { metadata: { name: 'e117-echo-mock' }, spec: { modelGroup: 'e117-echo', provider: 'mock', providerModel: 'mock', enabled: true } });
  // Gleicher Preis wie in aigw77.spec.js (409 = schon vorhanden).
  await ensure(request, 'prices', { spec: { provider: 'mock', model: 'mock', validFrom: '2025-01-01T00:00:00Z', perMillionTokens: { input: '1000', output: '2000' } } });
  const secrets = {};
  for (const [name, team] of [
    [KEY_A, ALPHA],
    [KEY_B, BETA],
  ]) {
    const k = await api(request, 'POST', 'api-keys', { metadata: { name }, spec: { organization: ORG, team } });
    expect(k.status).toBe(201);
    secrets[name] = k.body.secret;
  }
  const chat = (key, i) =>
    request.post('/v1/chat/completions', {
      headers: { authorization: `Bearer ${secrets[key]}`, 'content-type': 'application/json' },
      data: JSON.stringify({ model: 'e117-echo', messages: [{ role: 'user', content: `Kostenfrage ${i}: wie teuer ist diese Anfrage?` }] }),
    });
  // Neue Keys gelten erst nach dem Hot Reload – bis dahin 401.
  await expect.poll(async () => (await chat(KEY_A, 0)).status(), { timeout: 15_000 }).toBe(200);
  await expect.poll(async () => (await chat(KEY_B, 0)).status(), { timeout: 15_000 }).toBe(200);
  for (let i = 1; i < 6; i++) expect((await chat(KEY_A, i)).status()).toBe(200);
  for (let i = 1; i < 3; i++) expect((await chat(KEY_B, i)).status()).toBe(200);
  // Warten, bis der Writer die Rollups geschrieben hat.
  await expect
    .poll(async () => Number((await api(request, 'GET', `costs/summary?key=${KEY_A}`)).body?.requests ?? 0), { timeout: 20_000 })
    .toBeGreaterThanOrEqual(6);
});

test('Kosten-API: Kennzahlen, Treiber, Export, Budgets und Sichtregeln', async ({ request }) => {
  const s = await api(request, 'GET', `costs/summary?organization=${ORG}`);
  expect(s.status).toBe(200);
  expect(s.body.currency).toBe('USD');
  expect(Number(s.body.total.amount)).toBeGreaterThan(0);
  const parts = Object.fromEntries(s.body.breakdown.map((p) => [p.part, Number(p.amount)]));
  expect(parts.input).toBeGreaterThan(0);
  expect(parts.output).toBeGreaterThan(0);
  const d = await api(request, 'GET', `costs/breakdown?dimension=team&organization=${ORG}`);
  expect(d.body.items.map((i) => i.name)).toEqual(expect.arrayContaining([ALPHA, BETA]));
  const b = await api(request, 'GET', `costs/budgets?level=team&target=${ALPHA}`);
  expect(b.body.items).toHaveLength(1);
  expect(b.body.items[0].forecast.method).toBe('average');
  expect(b.body.items[0].burnDown.length).toBeGreaterThan(0);

  // Team-Betrachter: nur das eigene Team, fremdes Team → 403, Export ohne fremde Zeilen.
  const name = `e117-viewer-${RUN}`;
  const ak = await api(request, 'POST', 'admin-api-keys', { name, roles: [{ role: 'viewer', organization: ORG, team: BETA }] });
  expect(ak.status).toBe(201);
  const viewer = ak.body.secret;
  expect((await api(request, 'GET', `costs/summary?team=${ALPHA}`, undefined, viewer)).status).toBe(403);
  const own = await api(request, 'GET', 'costs/breakdown?dimension=team', undefined, viewer);
  expect(own.status).toBe(200);
  expect(own.body.items.map((i) => i.name)).toEqual([BETA]);
  const res = await request.get('/admin/api/v1/costs/export?dimension=team&granularity=total', { headers: { authorization: `Bearer ${viewer}` } });
  expect(res.status()).toBe(200);
  expect(res.headers()['content-type']).toContain('text/csv');
  const csv = await res.text();
  expect(csv).toContain(`,${BETA},`);
  expect(csv).not.toContain(ALPHA);
  await api(request, 'DELETE', `admin-api-keys/${name}`, undefined, TOKEN, { 'If-Match': '*' });
});

test('Seite Kosten: Kennzahlen, Verlauf, Drilldown, Budget, Export, Befehlspalette', async ({ page }) => {
  await useToken(page);
  await gotoReal(page, `/costs?range=24h&organization=${ORG}`);
  await expect(page.getByTestId('cost-kpis')).toBeVisible();
  await expect(page.getByTestId('kpi-total')).not.toContainText(/Gesamtkosten\s*0,00/);
  await expect(page.getByTestId('cost-filters')).toContainText(ORG);
  // Gestapelter Verlauf (LayerChart) und Tabellen-Alternative.
  await expect(page.getByTestId('cost-chart').getByTestId('stacked-area')).toBeVisible();
  await page.getByTestId('cost-chart-table').locator('summary').click();
  await expect(page.getByTestId('cost-chart-table').locator('table')).toContainText(ALPHA);
  await expect(page.getByTestId('cost-breakdown')).toContainText('Ausgabe');
  await expect(page.getByTestId('cost-savings')).toContainText('Schätzung');

  // Drilldown: Team → gefiltert, Gruppierung nach Key, Link in den Log-Explorer.
  const drivers = page.getByTestId('cost-drivers');
  await drivers.getByRole('button', { name: `Nach ${ALPHA} filtern` }).click();
  await expect(page).toHaveURL(new RegExp(`team=${ALPHA}`));
  await expect(page.getByTestId('cost-filters')).toContainText(ALPHA);
  await expect(drivers).toContainText('API-Key');
  await expect(drivers.getByTestId('driver-row')).toHaveCount(1);
  await expect(drivers.getByRole('link', { name: 'Logs' })).toHaveAttribute('href', `/admin/logs?key=${KEY_A}`);
  await page.getByRole('button', { name: 'Filter Team entfernen' }).click();
  await expect(page).not.toHaveURL(new RegExp(`team=${ALPHA}`));

  // Budget mit Prognose und Burn-down.
  const budgets = page.getByTestId('cost-budgets');
  await expect(budgets).toContainText(ALPHA);
  await expect(budgets).toContainText('Prognose');
  await budgets.getByRole('button', { name: ALPHA, exact: true }).click();
  await expect(page.getByTestId('cost-burndown').locator('figure')).toBeVisible();

  // Chargeback-Export (CSV) mit Kostenstelle und Labels.
  const exp = page.getByTestId('cost-export');
  await exp.getByRole('combobox', { name: 'Abschnitte' }).click();
  await page.getByRole('option', { name: 'Gesamt' }).click();
  const [dl] = await Promise.all([page.waitForEvent('download'), page.getByTestId('cost-export-button').click()]);
  expect(dl.suggestedFilename()).toMatch(/^aigw-kosten-team-.*\.csv$/);
  const text = fs.readFileSync(await dl.path(), 'utf8');
  expect(text.split('\r\n')[0]).toContain('cost_center');
  expect(text).toContain('label.bereich');
  const row = text.split('\r\n').find((l) => l.includes(`,${ALPHA},`));
  expect(row).toContain('CC-ALPHA');
  expect(row).toContain(',USD');

  // Befehlspalette findet die Seite.
  await page.keyboard.press('Control+k');
  await expect(page.getByTestId('command-palette')).toBeVisible();
  await page.keyboard.type('Chargeback');
  await expect(page.getByTestId('command-palette')).toContainText('Kosten');
  clean(page);
});

for (const [theme, mode] of [
  ['hell', 'light'],
  ['dunkel', 'dark'],
]) {
  test(`echt kosten ${theme}`, async ({ page }) => {
    // Hoher Viewport statt fullPage: so bleiben Topbar und Seitenleiste (sticky) an ihrem Platz.
    await page.setViewportSize({ width: 1440, height: 2080 });
    await useToken(page);
    await gotoReal(page, `/costs?range=24h&organization=${ORG}`, { theme: mode });
    await expect(page.getByTestId('cost-chart').getByTestId('stacked-area')).toBeVisible();
    await page.getByTestId('cost-budgets').getByRole('button', { name: ALPHA, exact: true }).click();
    await expect(page.getByTestId('cost-burndown').locator('figure')).toBeVisible();
    await page.waitForTimeout(400);
    await page.screenshot({ path: `${OUT}/screens/echt-kosten-${theme}-desktop.png` });
    clean(page);
  });
}
