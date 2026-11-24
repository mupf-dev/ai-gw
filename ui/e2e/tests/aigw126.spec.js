// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// AIGW-125/126 gegen das echte `aigw serve`: Budget je Deployment. Zwei Mock-„Konten“ (priority 0/1) mit Preis;
// das erste hat ein kleines Budget und wird nach Erschöpfung ohne Fehler übersprungen. Admin-API (usage/budgets
// level=deployment), UI: Budgetfelder im Deployment-Dialog, Verbrauch in Prozent auf der Detailseite, Strategie
// „Budget ausgleichen“ in der Modellgruppe, Deployment-Budget auf der Kosten-Seite. Ohne CSP-Verstöße.
import { api, clean, ensure, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const GROUP = 'e126-claude';
const A0 = 'e126-acct-0';
const A1 = 'e126-acct-1';
const ORG = 'e126-org';
const RUN = Date.now().toString(36);
let secret = '';

const chat = (request) =>
  request.post('/v1/chat/completions', {
    headers: { authorization: `Bearer ${secret}`, 'content-type': 'application/json' },
    data: JSON.stringify({ model: GROUP, max_tokens: 16, messages: [{ role: 'user', content: 'Budgetfrage: welches Konto antwortet?' }] }),
  });

test.beforeAll(async ({ request }) => {
  await ensure(request, 'model-groups', { metadata: { name: GROUP }, spec: { routing: 'weighted' } });
  // Gleicher Preis wie in aigw77/117 (409 = schon vorhanden).
  await ensure(request, 'prices', { spec: { provider: 'mock', model: 'mock', validFrom: '2025-01-01T00:00:00Z', perMillionTokens: { input: '1000', output: '2000' } } });
  await ensure(request, 'deployments', {
    metadata: { name: A0 },
    spec: { modelGroup: GROUP, provider: 'mock', providerModel: 'mock', priority: 0, budget: { amount: '0.05', period: 'total', warnAtPercent: [80], switchAtPercent: 98, enforcement: 'soft' } },
  });
  await ensure(request, 'deployments', {
    metadata: { name: A1 },
    spec: { modelGroup: GROUP, provider: 'mock', providerModel: 'mock', priority: 1, budget: { amount: '100', period: 'month' } },
  });
  await ensure(request, 'organizations', { metadata: { name: ORG }, spec: {} });
  const k = await api(request, 'POST', 'api-keys', { metadata: { name: `e126-key-${RUN}` }, spec: { organization: ORG } });
  expect(k.status).toBe(201);
  secret = k.body.secret;
});

test('Konto mit erschöpftem Budget wird ohne Fehler übersprungen', async ({ request }) => {
  // Neue Keys/Deployments gelten erst nach dem Hot Reload – bis dahin 401/404.
  await expect.poll(async () => (await chat(request)).status(), { timeout: 15_000 }).toBe(200);
  const seen = [];
  for (let i = 0; i < 12; i++) {
    const r = await chat(request);
    expect(r.status()).toBe(200);
    seen.push(r.headers()['x-aigw-deployment']);
  }
  expect(seen).toContain(A1);
  // Nach dem Wechsel bleibt es beim zweiten Konto.
  expect(seen.slice(seen.indexOf(A1))).not.toContain(A0);

  await expect
    .poll(async () => (await api(request, 'GET', 'usage/budgets?level=deployment')).body?.items?.find((i) => i.targetName === A0)?.percent ?? 0, { timeout: 15_000 })
    .toBeGreaterThanOrEqual(98);
  const b = (await api(request, 'GET', 'usage/budgets?level=deployment')).body.items.find((i) => i.targetName === A0);
  expect(b).toMatchObject({ level: 'deployment', modelGroup: GROUP, switchAtPercent: 98, amount: '0.05' });
  const m = await request.get('/metrics');
  if (m.ok()) expect(await m.text()).toContain(`aigw_deployment_budget_used_ratio{model_group="${GROUP}",deployment="${A0}"}`);
});

test('UI: Budget am Deployment, Verbrauch in Prozent, Strategie „Budget ausgleichen“', async ({ page }) => {
  await useToken(page);
  await gotoReal(page, `/deployments/${A0}`);
  await expect(page.getByTestId('deployment-detail')).toBeVisible();
  const card = page.getByTestId('deployment-budget-card');
  await expect(card).toContainText('0.05');
  await expect(card).toContainText('98 %');
  await expect(page.getByTestId('deployment-budget-usage')).toBeVisible({ timeout: 15_000 });
  await page.screenshot({ path: `${OUT}/screens/echt-deployment-budget-hell-desktop.png`, fullPage: true });

  await page.getByTestId('edit-deployment').click();
  const dlg = page.getByTestId('deployment-form-dialog');
  await expect(dlg.getByTestId('deployment-budget-amount')).toHaveValue('0.05');
  await expect(dlg.getByTestId('deployment-budget-switch')).toHaveValue('98');
  await dlg.getByTestId('deployment-budget-switch').fill('0');
  await page.getByTestId('deployment-form-submit').click();
  await expect(dlg.getByText('Ganze Zahl 1–100.')).toBeVisible();
  await dlg.getByRole('button', { name: 'Abbrechen' }).click();

  await gotoReal(page, `/models/${GROUP}`);
  await page.getByTestId('edit-group').click();
  const g = page.getByTestId('group-form-dialog');
  await g.getByRole('combobox', { name: 'Routing-Strategie' }).click();
  await expect(page.getByRole('option', { name: /Budget ausgleichen/ })).toBeVisible();
  await page.keyboard.press('Escape');

  await gotoReal(page, '/costs');
  await expect(page.getByTestId('cost-budgets')).toContainText(A0, { timeout: 15_000 });
  await expect(page.getByTestId('cost-budgets')).toContainText('Deployment');
  clean(page, { allow: [/422/] });
});
