// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// AIGW-84 gegen das echte `aigw serve`: Anbieter-Übersicht je Endpunkt (auch ohne Zugangsdaten, Entra ID per
// Managed Identity inline bzw. über `spec.entra`), Zustand aus dem Routing, Links zu Deployments; Zugangsdaten-
// Formular mit Anmeldeart „Entra ID“. Ohne CSP-Verstöße und Konsolenfehler.
import { api, clean, ensure, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const UAMI = '11111111-2222-3333-4444-555555555555';
const FOUNDRY = 'https://e84.services.ai.azure.com/anthropic';
const AOAI = 'https://e84.openai.azure.com';

test.beforeAll(async ({ request }) => {
  for (const g of ['e84-claude', 'e84-gpt']) await ensure(request, 'model-groups', { metadata: { name: g }, spec: {} });
  await ensure(request, 'provider-credentials', {
    metadata: { name: 'e84-foundry-mi' },
    spec: { provider: 'anthropic', entra: { credential: 'managedIdentity', clientId: UAMI } },
  });
  await ensure(request, 'deployments', {
    metadata: { name: 'e84-claude-foundry' },
    spec: { modelGroup: 'e84-claude', provider: 'anthropic', endpoint: FOUNDRY, providerModel: 'claude-sonnet-5-5', credential: 'e84-foundry-mi' },
  });
  // Live-Fall vor AIGW-84: Inline-Einstellungen, keine Zugangsdaten.
  await ensure(request, 'deployments', {
    metadata: { name: 'e84-gpt-inline' },
    spec: {
      modelGroup: 'e84-gpt',
      provider: 'azure-openai',
      endpoint: AOAI,
      providerModel: 'gpt-5',
      settings: { apiVersion: '2025-04-01-preview', auth: 'entraId', entraCredential: 'managedIdentity', entraClientId: UAMI },
    },
  });
});

test('API: Übersicht je Endpunkt mit Entra-Anmeldung und Laufzeitzustand', async ({ request }) => {
  await expect
    .poll(async () => {
      const r = await api(request, 'GET', 'providers');
      const it = r.body.items.find((i) => i.endpoint === FOUNDRY);
      return it?.deployments[0]?.state;
    })
    .toBe('ok');
  const r = await api(request, 'GET', 'providers');
  const claude = r.body.items.find((i) => i.endpoint === FOUNDRY);
  expect(claude.auth).toEqual([{ kind: 'entraId', entraCredential: 'managedIdentity', clientId: UAMI, credential: 'e84-foundry-mi' }]);
  const gpt = r.body.items.find((i) => i.endpoint === AOAI);
  expect(gpt.auth[0]).toMatchObject({ kind: 'entraId', entraCredential: 'managedIdentity' });
  expect(gpt.auth[0].credential).toBeUndefined();
  // Entra nur für Azure OpenAI und Anthropic.
  const bad = await api(request, 'POST', 'provider-credentials', { metadata: { name: 'e84-bad' }, spec: { provider: 'openai', entra: { credential: 'managedIdentity' } } });
  expect(bad.status).toBe(422);
  expect(bad.body.detail).toContain('spec.entra');
});

test('Seite „Anbieter“: Endpunkte ohne Zugangsdaten, Links, Formular „Entra ID“', async ({ page, request }) => {
  await useToken(page);
  await gotoReal(page, '/providers');
  const eps = page.getByTestId('provider-endpoints');
  const foundryRow = eps.getByRole('row').filter({ hasText: FOUNDRY });
  await expect(foundryRow).toContainText('Entra ID · Managed Identity');
  await expect(foundryRow).toContainText('e84-foundry-mi');
  await expect(foundryRow).toContainText('in Ordnung');
  const inlineRow = eps.getByRole('row').filter({ hasText: AOAI });
  await expect(inlineRow).toContainText('inline');
  await page.screenshot({ path: `${OUT}/screens/aigw84-providers.png`, fullPage: true });
  // Zugangsdaten-Tabelle zeigt die Entra-Anmeldung statt „keine Werte“.
  await expect(page.getByTestId('credentials-table').getByRole('row').filter({ hasText: 'e84-foundry-mi' })).toContainText('Entra ID · Managed Identity');

  // Neue Zugangsdaten mit Entra ID (Managed Identity, ohne Geheimnis).
  await page.getByTestId('create-credential').click();
  const d = page.getByTestId('credential-form-dialog');
  await d.getByRole('textbox', { name: /^Name/ }).fill('e84-aoai-mi');
  await d.getByRole('combobox', { name: /Anbieter/ }).click();
  await page.getByRole('option', { name: 'Azure OpenAI' }).click();
  await d.getByRole('radio', { name: /Managed Identity/ }).click();
  await expect(d.getByTestId('credential-entra')).toBeVisible();
  await d.getByRole('textbox', { name: /Client-ID/ }).fill(UAMI);
  await page.screenshot({ path: `${OUT}/screens/aigw84-credential-entra.png` });
  await d.getByTestId('credential-form-submit').click();
  await expect(d).toBeHidden();
  const c = await api(request, 'GET', 'provider-credentials/e84-aoai-mi');
  expect(c.body.spec).toMatchObject({ provider: 'azure-openai', entra: { credential: 'managedIdentity', clientId: UAMI }, fields: [] });

  // Link zum Deployment.
  await foundryRow.getByRole('link', { name: 'e84-claude-foundry' }).click();
  await expect(page).toHaveURL(/\/admin\/deployments\/e84-claude-foundry/);
  clean(page);
});
