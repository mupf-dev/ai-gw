// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-101 gegen das echte `aigw serve` (run.sh: wasm.trusted_keys = e2e-test, require_signature: false): WASM-Modul
// mit Ed25519-Signatur über die UI hochladen (Status „gültig“), falsche Signatur und unbekannter Schlüssel werden
// abgelehnt (422), unsignierte Module sind im Übergangsmodus erlaubt („unsigniert“). Ohne CSP-Verstöße.
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { api, clean, expect, gotoReal, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const RUN = Date.now().toString(36);
const WASM = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../../crates/aigw-wasm/tests/data/keyword-guard.wasm');
// Nur für Tests: öffentlicher Teil steht in run.sh (wasm.trusted_keys[e2e-test]).
const TEST_KEY = '-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIISeyP02UPhXd4EJLgZwJ3YqquNP2arCUOoBO3c+8ExW\n-----END PRIVATE KEY-----\n';
const bytes = fs.readFileSync(WASM);
const sha = crypto.createHash('sha256').update(bytes).digest('hex');
/** Signatur wie `aigw wasm sign`: Ed25519 über `aigw-wasm-module-v1:<sha256>`. */
const sign = (key, digest = sha) => crypto.sign(null, Buffer.from(`aigw-wasm-module-v1:${digest}`), key).toString('base64');

test('WASM-Signatur: signiert hochladen, Status in der Liste, Ablehnungen', async ({ page, request }) => {
  const mod = `e101-signed-${RUN}`;
  await useToken(page);
  await gotoReal(page, '/guardrails?tab=wasm');
  await expect(page.getByTestId('wasm-trust')).toContainText('e2e-test');
  await page.getByTestId('wasm-upload').click();
  const form = page.getByTestId('wasm-form');
  await form.getByTestId('wasm-file').setInputFiles(WASM);
  await form.getByRole('textbox', { name: 'Name' }).fill(mod);
  await expect(form.getByRole('textbox', { name: 'Schlüssel-ID' })).toHaveValue('e2e-test');
  await form.getByRole('textbox', { name: 'Signatur' }).fill(sign(TEST_KEY));
  await page.getByTestId('wasm-form-submit').click();
  await expect(form).toBeHidden();
  const row = page.getByTestId('wasm-row').filter({ hasText: mod });
  await expect(row.getByTestId('wasm-signature')).toContainText('gültig');
  await expect(row.getByTestId('wasm-signature')).toContainText('e2e-test');
  const info = await api(request, 'GET', `wasm-modules/${mod}`);
  expect(info.body.signature).toEqual({ keyId: 'e2e-test', status: 'valid', loadable: true });

  // Falsche Signatur (anderer Schlüssel) über die UI → Formularfehler, nichts ersetzt.
  const other = crypto.generateKeyPairSync('ed25519').privateKey;
  await page.getByRole('button', { name: `${mod} ersetzen` }).click();
  await form.getByTestId('wasm-file').setInputFiles(WASM);
  await form.getByRole('textbox', { name: 'Signatur' }).fill(sign(other));
  await page.getByTestId('wasm-form-submit').click();
  await expect(page.getByTestId('form-error')).toContainText('ungültig');
  await page.keyboard.press('Escape');

  // API: unbekannter Schlüssel → 422; unsigniert im Übergangsmodus erlaubt.
  const put = (name, query) =>
    request.fetch(`/admin/api/v1/wasm-modules/${name}?sha256=${sha}${query}`, {
      method: 'PUT',
      headers: { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/wasm' },
      data: bytes,
    });
  const unknown = await put(`e101-unknown-${RUN}`, `&keyId=fremd&signature=${encodeURIComponent(sign(TEST_KEY))}`);
  expect(unknown.status()).toBe(422);
  expect(await unknown.text()).toContain('unbekannter Schlüssel');
  const plain = await put(`e101-plain-${RUN}`, '');
  expect(plain.status()).toBe(201);
  expect((await plain.json()).signature).toEqual({ status: 'unsigned', loadable: true });
  await page.reload();
  await expect(page.getByTestId('wasm-row').filter({ hasText: `e101-plain-${RUN}` }).getByTestId('wasm-signature')).toContainText('unsigniert');

  // Aufräumen.
  for (const name of [mod, `e101-plain-${RUN}`]) {
    expect((await api(request, 'DELETE', `wasm-modules/${name}`, undefined, TOKEN, { 'If-Match': '*' })).status).toBe(204);
  }
  // Der absichtlich abgelehnte Upload (422) erscheint als Konsolenfehler.
  clean(page, { allow: [/status of 422/] });
});
