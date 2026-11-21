// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-115 gegen das echte `aigw serve` (run.sh konfiguriert den Aussteller `entra-e2e` unter auth.jwt):
// Workload-/Agent-Identitäten am Key per Formular anlegen und in der Detailansicht sehen, Eindeutigkeit über
// alle Keys (409), Formatprüfung (422), Datenebene: fremdes/ungültiges Access-Token → 401 ohne Details.
import { api, clean, ensure, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const ORG = 'e115-org';
const KEY = 'e115-agent';
const AGENT = '11112222-bbbb-3333-cccc-4444dddd5555';

test.beforeAll(async ({ request }) => {
  await ensure(request, 'organizations', { metadata: { name: ORG }, spec: {} });
  for (const k of [KEY, 'e115-other']) await api(request, 'DELETE', `api-keys/${k}`, undefined, TOKEN, { 'If-Match': '*' });
});

test('Key-Formular: Agent-Identität hinzufügen, Detailansicht, Eindeutigkeit', async ({ page, request }) => {
  await ensure(request, 'api-keys', { metadata: { name: KEY }, spec: { organization: ORG } });
  await useToken(page);
  await gotoReal(page, `/keys/${KEY}`);
  await page.getByTestId('key-detail').getByRole('button', { name: 'Bearbeiten' }).click();
  const dlg = page.getByTestId('key-form-dialog');
  const ed = dlg.getByTestId('workload-identities');
  await expect(ed).toBeVisible();
  await ed.getByTestId('add-workload-identity').click();
  // Aussteller aus auth.jwt ist vorausgewählt.
  await expect(ed.getByRole('combobox', { name: 'Aussteller 1' })).toContainText('entra-e2e');
  await ed.getByRole('textbox', { name: 'Kennung 1' }).fill(AGENT);
  await ed.getByRole('textbox', { name: 'Beschreibung 1' }).fill('Support-Agent');
  await ed.scrollIntoViewIfNeeded();
  await page.screenshot({ path: `${OUT}/screens/aigw115-key-identities.png` });
  await dlg.getByTestId('key-form-submit').click();
  await expect(dlg).toBeHidden();
  const k = await api(request, 'GET', `api-keys/${KEY}`);
  expect(k.body.spec.workloadIdentities).toEqual([{ issuer: 'entra-e2e', subject: AGENT, description: 'Support-Agent' }]);
  await expect(page.getByTestId('key-workload-identities')).toContainText(AGENT);

  // Dieselbe Identität an einem zweiten Key → 409, ungültiger Aussteller-Name → 422.
  const dup = await api(request, 'POST', 'api-keys', {
    metadata: { name: 'e115-other' },
    spec: { organization: ORG, workloadIdentities: [{ issuer: 'entra-e2e', subject: AGENT.toUpperCase() }] },
  });
  expect(dup.status).toBe(409);
  expect(dup.body.detail).toContain(KEY);
  const bad = await api(request, 'POST', 'api-keys', {
    metadata: { name: 'e115-other' },
    spec: { organization: ORG, workloadIdentities: [{ issuer: 'Entra E2E', subject: 'x' }] },
  });
  expect(bad.status).toBe(422);

  // Entfernen über das Formular.
  await page.getByTestId('key-detail').getByRole('button', { name: 'Bearbeiten' }).click();
  await dlg.getByRole('button', { name: 'Identität 1 entfernen' }).click();
  await dlg.getByTestId('key-form-submit').click();
  await expect(dlg).toBeHidden();
  const k2 = await api(request, 'GET', `api-keys/${KEY}`);
  expect(k2.body.spec.workloadIdentities ?? []).toEqual([]);
  await expect(page.getByTestId('key-workload-identities')).toHaveCount(0);
  clean(page);
});

test('Datenebene: ungültiges Access-Token → 401 ohne Details', async ({ request }) => {
  const h = { alg: 'RS256', kid: 'x' };
  const c = { iss: 'https://sts.windows.net/72f988bf-86f1-41af-91ab-2d7cd011db47/', aud: 'api://ai-gw-e2e', appid: AGENT, exp: 4102444800 };
  const b64 = (o) => Buffer.from(JSON.stringify(o)).toString('base64url');
  const token = `${b64(h)}.${b64(c)}.c2ln`;
  const res = await request.fetch('/v1/models', { headers: { authorization: `Bearer ${token}` } });
  expect(res.status()).toBe(401);
  const text = await res.text();
  expect(text).not.toContain(AGENT);
  expect(text).not.toContain(token);
});
