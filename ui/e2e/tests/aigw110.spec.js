// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// AIGW-110/114 gegen das echte `aigw serve`: strukturierte Formulare der Guardrails `link-guard` und `language`,
// Testlauf (Ausgabe: fremdes Bild wird entschärft; Eingabe: Sprache abgelehnt), Speichern. Ohne CSP-Verstöße.
import { api, clean, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const RUN = Date.now().toString(36);

async function pick(page, scope, name, option) {
  await scope.getByRole('combobox', { name, exact: true }).click();
  await page.getByRole('option', { name: option, exact: true }).click();
}

test('Guardrails: link-guard- und language-Formular mit Testlauf', async ({ page, request }) => {
  const lg = `e110-lg-${RUN}`;
  const lang = `e110-lang-${RUN}`;
  await useToken(page);
  await gotoReal(page, '/guardrails');

  // link-guard: erlaubte Domain, Aktion defang, Testlauf auf der Ausgabe.
  await page.getByTestId('create-binding').click();
  const dlg = page.getByTestId('binding-form');
  await dlg.getByRole('textbox', { name: 'Name' }).fill(lg);
  await pick(page, dlg, 'Plugin', 'link-guard');
  await dlg.getByRole('textbox', { name: 'Erlaubte Domains' }).fill('docs.example.com\n*.cdn.example.com');
  await pick(page, dlg, 'Aktion', 'defang – Platzhalter');
  await pick(page, page.getByTestId('plugin-test'), 'Phase', 'Ausgabe (Antwort)');
  await page.getByTestId('plugin-test-input').fill('Hier ![x](https://evil.test/p.png?d=1) und ![y](https://img.cdn.example.com/y.png)');
  await page.getByTestId('plugin-test-run').click();
  const out = page.getByTestId('plugin-test-output');
  await expect(out).toContainText('[blocked image: evil[.]test]');
  await expect(out).toContainText('https://img.cdn.example.com/y.png');
  await expect(page.getByTestId('plugin-test-result')).toContainText('image · defang');
  await page.screenshot({ path: `${OUT}/screens/aigw110-link-guard.png` });
  await page.getByTestId('binding-form-submit').click();
  await expect(dlg).toBeHidden();
  let b = await api(request, 'GET', `plugin-bindings/${lg}`);
  expect(b.status).toBe(200);
  expect(b.body.spec.config).toEqual({ allowDomains: ['docs.example.com', '*.cdn.example.com'], action: 'defang' });

  // language: nur Deutsch, Testlauf auf der Eingabe lehnt Englisch ab.
  await page.getByTestId('create-binding').click();
  await dlg.getByRole('textbox', { name: 'Name' }).fill(lang);
  await pick(page, dlg, 'Plugin', 'language');
  await dlg.getByRole('textbox', { name: 'Erlaubte Sprachen' }).fill('de');
  await page.getByTestId('plugin-test-input').fill('Please summarize the following report about the quarterly figures briefly.');
  await page.getByTestId('plugin-test-run').click();
  await expect(page.getByTestId('plugin-test-result')).toContainText('abgelehnt');
  await expect(page.getByTestId('plugin-test-result')).toContainText('language_not_allowed');
  await page.getByTestId('plugin-test-input').fill('Bitte fasse mir den folgenden Bericht über die Quartalszahlen kurz zusammen.');
  await page.getByTestId('plugin-test-run').click();
  await expect(page.getByTestId('plugin-test-result')).toContainText('unverändert');
  await page.getByTestId('binding-form-submit').click();
  await expect(dlg).toBeHidden();
  b = await api(request, 'GET', `plugin-bindings/${lang}`);
  expect(b.status).toBe(200);
  expect(b.body.spec.config).toEqual({ allow: ['de'] });

  for (const n of [lg, lang]) await api(request, 'DELETE', `plugin-bindings/${n}`, undefined, TOKEN, { 'If-Match': '*' });
  clean(page);
});
