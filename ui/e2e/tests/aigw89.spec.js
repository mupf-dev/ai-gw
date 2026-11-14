// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-89 gegen das echte `aigw serve`: Guardrail-Profil „Streng“ mit Azure AI Content Safety über Entra ID
// (Zugang „Entra ID“, Anbieter-Zugangsdaten mit spec.entra) – Diff, Anwenden, gespeicherte Bindung mit
// `auth: entraId` + `credential`. Ohne CSP-Verstöße.
import { api, clean, ensure, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const ORG = 'e89-org';
const TEAM = 'e89-team';
const CRED = 'e89-cs-mi';

test.beforeAll(async ({ request }) => {
  await ensure(request, 'organizations', { metadata: { name: ORG }, spec: { displayName: 'AIGW-89 Org' } });
  await ensure(request, 'teams', { metadata: { name: TEAM }, spec: { organization: ORG, displayName: 'AIGW-89 Team' } });
  await ensure(request, 'provider-credentials', { metadata: { name: CRED }, spec: { provider: 'azure-openai', entra: { credential: 'managedIdentity' } } });
});

async function pick(page, scope, name, option) {
  await scope.getByRole('combobox', { name, exact: true }).click();
  await page.getByRole('option', { name: option, exact: true }).click();
}

test('Profil „Streng“: content-safety mit Entra ID anwenden', async ({ page, request }) => {
  await useToken(page);
  await gotoReal(page, '/guardrails?tab=profiles');
  await page.getByTestId('apply-streng').click();
  const dlg = page.getByTestId('profile-apply');
  await dlg.getByRole('combobox', { name: 'Ziel' }).fill(TEAM);
  await pick(page, dlg, 'Externe Inhaltsprüfung', 'Azure AI Content Safety');
  await dlg.getByLabel('Endpunkt').fill('https://e89.cognitiveservices.azure.com');
  await pick(page, dlg, 'Zugang', 'Entra ID (Anbieter-Zugangsdaten)');
  await expect(dlg.getByLabel('Secret')).toHaveCount(0);
  await dlg.getByLabel('Zugangsdaten (Entra)').fill(CRED);
  await page.screenshot({ path: `${OUT}/screens/aigw89-profil-entra.png` });
  await page.getByTestId('profile-apply-submit').click();
  await expect(page.getByTestId('profile-diff')).toContainText(`streng-content-safety-team-${TEAM}`);
  await page.getByTestId('profile-apply-submit').click();
  await expect(dlg).toBeHidden();

  const b = await api(request, 'GET', `plugin-bindings/streng-content-safety-team-${TEAM}`);
  expect(b.status).toBe(200);
  expect(b.body.spec.config).toMatchObject({ provider: 'azure', auth: 'entraId', credential: CRED, endpoint: 'https://e89.cognitiveservices.azure.com' });
  expect(b.body.spec.config.secret).toBeUndefined();

  // Aufräumen: Bindungen des Profils entfernen.
  for (const p of ['pii', 'prompt-filter', 'content-safety']) {
    await api(request, 'DELETE', `plugin-bindings/streng-${p}-team-${TEAM}`, undefined, TOKEN, { 'If-Match': '*' });
  }
  clean(page);
});
