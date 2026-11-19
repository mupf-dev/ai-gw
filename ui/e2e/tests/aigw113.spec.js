// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-113 gegen das echte `aigw serve`: Plugin-Art `schema-guard` im Katalog und im strukturierten Formular
// (Aktion retry mit Wiederholungen, Stream puffern), Backend-Prüfung der Obergrenze am Feld. Ohne CSP-Verstöße.
import { api, clean, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const RUN = Date.now().toString(36);

async function pick(page, scope, name, option) {
  await scope.getByRole('combobox', { name, exact: true }).click();
  await page.getByRole('option', { name: option, exact: true }).click();
}

test('Guardrails: schema-guard im Katalog und Formular', async ({ page, request }) => {
  const cat = await api(request, 'GET', 'plugins');
  expect(cat.status).toBe(200);
  const sg = cat.body.items.find((p) => p.name === 'schema-guard');
  expect(sg).toMatchObject({ defaultFailMode: 'closed', defaultTimeoutMs: 60000, testable: false });

  const name = `e113-sg-${RUN}`;
  await useToken(page);
  await gotoReal(page, '/guardrails');
  await page.getByTestId('create-binding').click();
  const dlg = page.getByTestId('binding-form');
  await dlg.getByRole('textbox', { name: 'Name' }).fill(name);
  await pick(page, dlg, 'Plugin', 'schema-guard');
  await pick(page, dlg, 'Aktion', 'retry – neu anfragen, dann ablehnen');
  await pick(page, dlg, 'Stream', 'buffer – puffern (alle Aktionen)');
  await pick(page, dlg, 'Tool-Argumente', 'all – alle Tool-Aufrufe');
  // Obergrenze der Wiederholungen: Fehler des Backends am Formular.
  await dlg.getByRole('textbox', { name: 'Wiederholungen' }).fill('5');
  await page.getByTestId('binding-form-submit').click();
  await expect(dlg.getByText(/maxRetries muss zwischen 1 und 3/)).toBeVisible();
  await dlg.getByRole('textbox', { name: 'Wiederholungen' }).fill('2');
  await page.screenshot({ path: `${OUT}/screens/aigw113-schema-guard.png` });
  await page.getByTestId('binding-form-submit').click();
  await expect(dlg).toBeHidden();

  const b = await api(request, 'GET', `plugin-bindings/${name}`);
  expect(b.status).toBe(200);
  expect(b.body.spec.plugin).toBe('schema-guard');
  expect(b.body.spec.config).toEqual({ action: 'retry', maxRetries: 2, stream: 'buffer', tools: 'all' });
  expect(b.body.effective.failMode).toBe('closed');

  // Externe $ref-Prüfung gehört zur Anfrage, Konfigurationsfehler zur Bindung (422).
  const bad = await api(request, 'POST', 'plugin-bindings', {
    metadata: { name: `${name}-bad` },
    spec: { plugin: 'schema-guard', scope: { level: 'global' }, config: { action: 'flag', maxRetries: 2 } },
  });
  expect(bad.status).toBe(422);

  await api(request, 'DELETE', `plugin-bindings/${name}`, undefined, TOKEN, { 'If-Match': '*' });
  clean(page, { allow: [/status of 422/] });
});
