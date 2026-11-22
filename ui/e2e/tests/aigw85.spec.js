// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// AIGW-85 gegen das echte `aigw serve` mit Fake-Retail-API (run.sh, fake-retail.py): Preisvorschläge für ein
// Azure-Deployment (Azure Retail Prices, swedencentral, global) und ein OpenAI-Deployment (Preiskatalog), Hinweis auf
// Deployments ohne Preis, Übernahme in der UI, manueller Preis nur mit Bestätigung. Ohne CSP-Verstöße.
import { api, clean, ensure, expect, gotoReal, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });
test.describe.configure({ mode: 'serial' });

const GROUP = 'e85-gpt';

test.beforeAll(async ({ request }) => {
  const fake = (s) => ({ value: `${s}-nur-fuer-tests-0123456789` });
  await ensure(request, 'provider-credentials', { metadata: { name: 'e85-az' }, spec: { provider: 'azure-openai', data: { apiKey: fake('az') } } });
  await ensure(request, 'provider-credentials', { metadata: { name: 'e85-oa' }, spec: { provider: 'openai', data: { apiKey: fake('sk') } } });
  await ensure(request, 'model-groups', { metadata: { name: GROUP }, spec: {} });
  await ensure(request, 'deployments', {
    metadata: { name: 'e85-az-se' },
    spec: {
      modelGroup: GROUP,
      provider: 'azure-openai',
      providerModel: 'e85gpt54mini',
      credential: 'e85-az',
      endpoint: 'https://e85.openai.azure.com',
      settings: { apiVersion: '2024-10-21', pricing: { region: 'swedencentral', model: 'gpt-5.4-mini' } },
    },
  });
  await ensure(request, 'deployments', { metadata: { name: 'e85-oa' }, spec: { modelGroup: GROUP, provider: 'openai', providerModel: 'gpt-5.4-nano', credential: 'e85-oa' } });
});

test('API: Vorschläge aus Retail-API und Katalog', async ({ request }) => {
  const r = await api(request, 'GET', `price-suggestions?modelGroup=${GROUP}`);
  expect(r.status).toBe(200);
  const az = r.body.items.find((i) => i.deployment === 'e85-az-se');
  expect(az).toMatchObject({ source: 'azure-retail', region: 'swedencentral', deploymentType: 'global', status: 'missing' });
  expect(az.suggested.perMillionTokens).toEqual({ input: '0.75', cachedInput: '0.075', output: '4.5' });
  const oa = r.body.items.find((i) => i.deployment === 'e85-oa');
  expect(oa).toMatchObject({ source: 'catalog', status: 'missing' });
  expect(r.body.sources.find((s) => s.source === 'azure-retail')).toMatchObject({ scope: 'swedencentral', entries: 4 });
  // Unbekanntes Deployment → 422.
  expect((await api(request, 'POST', 'price-suggestions:accept', { items: [{ deployment: 'e85-nope' }] })).status).toBe(422);
});

test('UI: Hinweis ohne Preis, Auswahl und Übernahme', async ({ page, request }) => {
  await useToken(page);
  await gotoReal(page, '/prices');
  const card = page.getByTestId('price-suggestions-card');
  await expect(card).toBeVisible();
  await expect(page.getByTestId('unpriced-hint')).toContainText('e85-az-se');
  // Nur die eigenen Deployments übernehmen (andere Tests legen weitere an).
  const boxes = card.locator('[data-testid^="suggest-select-"]');
  await expect(boxes.first()).toBeVisible();
  for (const b of await boxes.all()) {
    const id = await b.getAttribute('data-testid');
    if (!id.includes('e85-')) await b.uncheck();
  }
  await expect(card.getByTestId('suggest-select-e85-az-se')).toBeChecked();
  await expect(card.getByTestId('suggest-select-e85-oa')).toBeChecked();
  await expect(card.getByTestId('suggest-accept')).toContainText('Übernehmen (2)');
  await card.getByTestId('suggest-accept').click();
  const dlg = page.getByTestId('suggest-confirm');
  await expect(dlg).toBeVisible();
  await dlg.getByRole('button', { name: 'Übernehmen' }).click();
  await expect(dlg).toBeHidden();
  await expect(card.getByRole('row', { name: /e85-az-se/ })).toContainText('aktuell');
  await expect(page.getByTestId('prices-table')).toContainText('e85gpt54mini');

  const prices = await api(request, 'GET', 'prices?model=e85gpt54mini');
  expect(prices.body.items[0].metadata.labels).toMatchObject({ 'aigw.io/price-source': 'azure-retail', 'aigw.io/price-region': 'swedencentral', 'aigw.io/price-deployment': 'e85-az-se' });
  clean(page);
});

test('Manueller Preis hat Vorrang', async ({ request }) => {
  // Manuelle Version (ohne Label) für das OpenAI-Modell – jünger als die übernommene.
  const validFrom = new Date(Date.now() + 2000).toISOString();
  const m = await api(request, 'POST', 'prices', { spec: { provider: 'openai', model: 'gpt-5.4-nano', validFrom, perMillionTokens: { input: '9', output: '9' } } });
  expect(m.status).toBe(201);
  await new Promise((r) => setTimeout(r, 3500));
  const r = await api(request, 'GET', 'price-suggestions?deployment=e85-oa');
  expect(r.body.items[0]).toMatchObject({ status: 'differs', current: { manual: true } });
  const no = await api(request, 'POST', 'price-suggestions:accept', { items: [{ deployment: 'e85-oa' }] });
  expect(no.status).toBe(409);
  const yes = await api(request, 'POST', 'price-suggestions:accept', { items: [{ deployment: 'e85-oa', overrideManual: true }] });
  expect(yes.status).toBe(200);
  expect(yes.body.created).toHaveLength(1);
});
