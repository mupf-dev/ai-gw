// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// AIGW-94 gegen das echte `aigw serve`: Zugangsdaten „Service Principal (Zertifikat)“ mit optionalem Passwort für
// verschlüsselte Schlüssel und Schalter „Zertifikatskette mitsenden (x5c)“ (`spec.entra.sendX5c`).
import { api, clean, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const TENANT = '72f988bf-86f1-41af-91ab-2d7cd011db47';
const CLIENT = '11111111-2222-3333-4444-555555555555';

test('Zugangsdaten: Zertifikat mit Passwort und x5c', async ({ page, request }) => {
  await api(request, 'DELETE', 'provider-credentials/e94-cert', undefined, TOKEN, { 'If-Match': '*' });
  await useToken(page);
  await gotoReal(page, '/providers');
  await page.getByTestId('create-credential').click();
  const d = page.getByTestId('credential-form-dialog');
  await d.getByRole('textbox', { name: /^Name/ }).fill('e94-cert');
  await d.getByRole('combobox', { name: /Anbieter/ }).click();
  await page.getByRole('option', { name: 'Azure OpenAI' }).click();
  await d.getByRole('radio', { name: /Service Principal \(Zertifikat\)/ }).click();
  const entra = d.getByTestId('credential-entra');
  await expect(entra).toBeVisible();
  await d.getByRole('textbox', { name: /Client-ID/ }).fill(CLIENT);
  await d.getByRole('textbox', { name: /Tenant-ID/ }).fill(TENANT);
  const x5c = d.getByTestId('credential-entra-x5c').getByRole('checkbox');
  await expect(x5c).not.toBeChecked();
  await x5c.click();
  await expect(x5c).toBeChecked();
  const fields = d.getByTestId('credential-fields');
  await fields.getByRole('textbox').first().fill('-----BEGIN CERTIFICATE-----\nMAMCAQc=\n-----END CERTIFICATE-----');
  await fields.getByLabel(/Client Certificate Password/).fill('pw-nur-test');
  await page.screenshot({ path: `${OUT}/screens/aigw94-credential-cert.png` });
  await d.getByTestId('credential-form-submit').click();
  await expect(d).toBeHidden();
  const c = await api(request, 'GET', 'provider-credentials/e94-cert');
  expect(c.body.spec.entra).toMatchObject({ credential: 'clientCertificate', tenantId: TENANT, clientId: CLIENT, sendX5c: true });
  expect([...c.body.spec.fields].sort()).toEqual(['clientCertificate', 'clientCertificatePassword']);
  expect(JSON.stringify(c.body)).not.toContain('pw-nur-test');

  // x5c/Passwort nur beim Zertifikat.
  const bad = await api(request, 'POST', 'provider-credentials', {
    metadata: { name: 'e94-bad' },
    spec: { provider: 'azure-openai', entra: { credential: 'managedIdentity', sendX5c: true } },
  });
  expect(bad.status).toBe(422);
  expect(bad.body.detail).toContain('sendX5c');
  await api(request, 'DELETE', 'provider-credentials/e94-cert', undefined, TOKEN, { 'If-Match': '*' });
  clean(page);
});
