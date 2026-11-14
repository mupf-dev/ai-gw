// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-63 gegen das echte `aigw serve`: WASM-Module hochladen (SHA-256 im Browser berechnet), Bindung der Art
// `wasm` mit Modulauswahl anlegen, Löschen des verwendeten Moduls wird abgelehnt (409), danach löschen. Dazu die
// Rechteprüfung des Uploads (Viewer-Key: 403) über die API. Ohne CSP-Verstöße.
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { api, clean, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const RUN = Date.now().toString(36);
const WASM = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../../crates/aigw-wasm/tests/data/keyword-guard.wasm');

async function pick(page, scope, name, option) {
  await scope.getByRole('combobox', { name, exact: true }).click();
  await page.getByRole('option', { name: option, exact: true }).click();
}

test('WASM-Module: hochladen, binden, Löschschutz, löschen', async ({ page, request }) => {
  const mod = `e63-guard-${RUN}`;
  const binding = `e63-wasm-${RUN}`;
  await useToken(page);
  await gotoReal(page, '/guardrails?tab=wasm');
  await page.getByTestId('wasm-upload').click();
  const form = page.getByTestId('wasm-form');
  await form.getByTestId('wasm-file').setInputFiles(WASM);
  await form.getByRole('textbox', { name: 'Name' }).fill(mod);
  await form.getByRole('textbox', { name: 'Beschreibung' }).fill('Stichwort-Blocker (e2e)');
  await page.getByRole('button', { name: 'Hochladen' }).click();
  await expect(form).toBeHidden();
  const row = page.getByTestId('wasm-row').filter({ hasText: mod });
  await expect(row).toBeVisible();
  const info = await api(request, 'GET', `wasm-modules/${mod}`);
  expect(info.status).toBe(200);
  expect(info.body.sha256).toMatch(/^[0-9a-f]{64}$/);
  await expect(row).toContainText(info.body.sha256.slice(0, 16));
  await page.screenshot({ path: `${OUT}/screens/echt-wasm-module-hell-desktop.png`, fullPage: true });

  // Bindung der Art wasm mit Modulauswahl und Konfiguration für das Modul.
  await page.getByTestId('tab-bindings').click();
  await page.getByTestId('create-binding').click();
  const dlg = page.getByTestId('binding-form');
  await dlg.getByRole('textbox', { name: 'Name' }).fill(binding);
  await pick(page, dlg, 'Plugin', 'wasm');
  await pick(page, dlg, 'Modul', mod);
  await page.getByTestId('wasm-config').fill('{"block": ["geheimprojekt"]}');
  await page.getByRole('button', { name: 'Anlegen' }).click();
  await expect(dlg).toBeHidden();
  const b = await api(request, 'GET', `plugin-bindings/${binding}`);
  expect(b.status).toBe(200);
  expect(b.body.spec.config).toEqual({ module: mod, config: { block: ['geheimprojekt'] } });
  expect(b.body.effective.failMode).toBe('closed');

  // Verwendet → Löschen abgelehnt (409).
  await page.getByTestId('tab-wasm').click();
  await expect(row).toContainText(binding);
  await page.getByTestId(`wasm-delete-${mod}`).click();
  await page.getByTestId('wasm-delete-dialog').getByRole('button', { name: 'Löschen' }).click();
  await expect(page.getByTestId('wasm-delete-dialog')).toContainText(/verwendet/);
  await page.keyboard.press('Escape');

  // Bindung weg → Modul löschbar.
  expect((await api(request, 'DELETE', `plugin-bindings/${binding}`, undefined, TOKEN, { 'If-Match': '*' })).status).toBe(204);
  await page.reload();
  await expect(row).toBeVisible();
  await page.getByTestId(`wasm-delete-${mod}`).click();
  await page.getByTestId('wasm-delete-dialog').getByRole('button', { name: 'Löschen' }).click();
  await expect(row).toHaveCount(0);
  expect((await api(request, 'GET', `wasm-modules/${mod}`)).status).toBe(404);
  clean(page);
});

test('WASM-Module: Upload nur mit passendem SHA-256 und nur für globale Admins', async ({ request }) => {
  const fs = await import('node:fs');
  const bytes = fs.readFileSync(WASM);
  const put = (name, sha, token = TOKEN) =>
    request.fetch(`/admin/api/v1/wasm-modules/${name}?sha256=${sha}`, {
      method: 'PUT',
      headers: { authorization: `Bearer ${token}`, 'content-type': 'application/wasm' },
      data: bytes,
    });
  const bad = await put(`e63-bad-${RUN}`, '0'.repeat(64));
  expect(bad.status()).toBe(422);
  expect(await bad.text()).toContain('sha256 stimmt nicht');
  // Viewer-Key (Admin-API-Key mit Rolle viewer) darf nicht hochladen.
  const key = await api(request, 'POST', 'admin-api-keys', { name: `e63-viewer-${RUN}`, description: 'AIGW-63 nur lesen', roles: [{ role: 'viewer' }] });
  expect(key.status).toBe(201);
  const viewer = key.body.secret;
  expect(typeof viewer).toBe('string');
  const crypto = await import('node:crypto');
  const sha = crypto.createHash('sha256').update(bytes).digest('hex');
  expect((await put(`e63-viewer-${RUN}`, sha, viewer)).status()).toBe(403);
  // Lesen darf der Viewer.
  expect((await api(request, 'GET', 'wasm-modules', undefined, viewer)).status).toBe(200);
});
