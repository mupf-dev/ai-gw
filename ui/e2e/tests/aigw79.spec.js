// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// AIGW-79 (S-09/F203) gegen das echte `aigw serve`: Payload-Richtlinie an Organisation/Team (payloadModeMax,
// payloadModeLocked). Serverseitig 422 mit feldgenauer Meldung, in den Formularen Stufen über dem Maximum nicht
// wählbar, gesperrte Felder deaktiviert mit Hinweis, Maximum in der Detailansicht. Ohne CSP-Verstöße.
import { api, clean, ensure, expect, gotoReal, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const ORG = 'e79-org';
const TEAM = 'e79-team';
const LOCKED = 'e79-locked';
const KEY = 'e79-key';

test.beforeAll(async ({ request }) => {
  await ensure(request, 'organizations', { metadata: { name: ORG }, spec: { payloadModeMax: 'metadata' } });
  await ensure(request, 'teams', { metadata: { name: TEAM }, spec: { organization: ORG } });
  await ensure(request, 'teams', { metadata: { name: LOCKED }, spec: { organization: ORG, payloadMode: 'off', payloadModeLocked: true } });
  await ensure(request, 'api-keys', { metadata: { name: KEY }, spec: { organization: ORG, team: LOCKED } });
});

test('API: Key über dem Maximum bzw. unter Sperre → 422 mit Feld', async ({ request }) => {
  const over = await api(request, 'POST', 'api-keys', { metadata: { name: 'e79-over' }, spec: { organization: ORG, team: TEAM, payloadMode: 'full' } });
  expect(over.status).toBe(422);
  expect(over.body.detail).toContain('payloadMode: full überschreitet die Obergrenze payloadModeMax = metadata');
  expect(over.body.violations[0]).toMatchObject({ name: 'e79-over', field: 'payloadMode' });
  const locked = await api(request, 'POST', 'api-keys', { metadata: { name: 'e79-lk' }, spec: { organization: ORG, team: LOCKED, payloadMode: 'off' } });
  expect(locked.status).toBe(422);
  expect(locked.body.detail).toContain('gesperrt');
  const team = await api(request, 'POST', 'teams', { metadata: { name: 'e79-big' }, spec: { organization: ORG, payloadModeMax: 'full' } });
  expect(team.status).toBe(422);
  expect(team.body.violations[0]).toMatchObject({ field: 'payloadModeMax' });
});

test('Formulare: Maximum anzeigen, Stufen darüber gesperrt, gesperrtes Feld deaktiviert', async ({ page, request }) => {
  await useToken(page);
  // Team-Detail zeigt das Maximum der Organisation.
  await gotoReal(page, `/teams/${TEAM}`);
  await expect(page.getByTestId('eff-payload-policy')).toContainText(`Maximum: nur Metadaten (Organisation ${ORG})`);
  // Team-Formular: „vollständig“ liegt über dem Maximum und ist nicht wählbar; Richtlinienfelder vorhanden.
  await page.getByTestId('edit-entity').click();
  const form = page.getByTestId('entity-form');
  await expect(form.getByText(`Maximum: Organisation ${ORG}`)).toBeVisible();
  await form.getByRole('combobox', { name: 'Payload-Logging' }).click();
  await expect(page.getByRole('option', { name: /^vollständig/ })).toHaveAttribute('data-disabled', '');
  await expect(page.getByRole('option', { name: /^nur Metadaten/ })).not.toHaveAttribute('data-disabled', '');
  await page.getByRole('option', { name: /^aus/ }).click();
  await form.getByRole('combobox', { name: 'Payload-Maximum' }).click();
  await expect(page.getByRole('option', { name: /^höchstens maskiert/ })).toHaveAttribute('data-disabled', '');
  await page.getByRole('option', { name: /^höchstens Metadaten|^höchstens nur Metadaten/ }).click();
  await form.getByRole('combobox', { name: 'Payload-Modus darunter' }).click();
  await page.getByRole('option', { name: 'gesperrt' }).click();
  await page.getByTestId('entity-form-submit').click();
  await expect(form).toBeHidden();
  const t = await api(request, 'GET', `teams/${TEAM}`);
  expect(t.body.spec).toMatchObject({ payloadMode: 'off', payloadModeMax: 'metadata', payloadModeLocked: true });

  // Key im gesperrten Team: Payload-Feld deaktiviert mit Hinweis.
  await gotoReal(page, `/keys/${KEY}`);
  await page.getByTestId('key-detail').getByRole('button', { name: 'Bearbeiten' }).click();
  const dlg = page.getByTestId('key-form-dialog');
  const pm = dlg.getByRole('combobox', { name: 'Payload-Logging' });
  await expect(pm).toBeDisabled();
  await expect(dlg.getByText(`Gesperrt durch Team ${LOCKED}`)).toBeVisible();
  clean(page);
});
