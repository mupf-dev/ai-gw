// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-106 gegen das echte `aigw serve` (run.sh: dritter Server mit wasm.four_eyes und wasm.sigstore): Upload über die
// UI wird nur eingereicht („wartet auf Freigabe“), der Hochlader selbst kann nicht freigeben (Hinweis statt Button,
// API 403); ein zweiter Admin (Admin-API-Key) gibt über den Button frei → aktiv, Herkunft „freigegeben von“. Verwerfen
// einer zweiten Einreichung. Sigstore-Bundle eines fremden Artefakts wird abgelehnt (422). Ohne CSP-Verstöße.
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { api, clean, expect, test, TOKEN, useToken } from './fixtures.js';

const FOUR = process.env.AIGW_FOUR_EYES_URL;
test.skip(!TOKEN || !FOUR, 'AIGW_TOKEN/AIGW_FOUR_EYES_URL fehlen (run.sh startet den dritten Server)');
test.use({ reducedMotion: 'reduce' });

const RUN = Date.now().toString(36);
const HERE = path.dirname(fileURLToPath(import.meta.url));
const WASM = path.resolve(HERE, '../../../crates/aigw-wasm/tests/data/keyword-guard.wasm');
const BUNDLE = path.resolve(HERE, '../../../crates/aigw-sigstore/tests/data/conformance/happy-path-v0.3/bundle.sigstore.json');
const bytes = fs.readFileSync(WASM);
const sha = crypto.createHash('sha256').update(bytes).digest('hex');

async function open(page, token) {
  await useToken(page, token);
  await page.goto(`${FOUR}/admin/guardrails?tab=wasm&theme=light`);
  await expect(page.locator('main h1').first()).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId('wasm-four-eyes')).toBeVisible();
}

test('Vier-Augen-Freigabe: einreichen, Selbstfreigabe gesperrt, zweiter Admin gibt frei', async ({ page, browser, playwright }) => {
  const mod = `e106-guard-${RUN}`;
  const ctx = await playwright.request.newContext({ baseURL: FOUR });
  const key = await api(ctx, 'POST', 'admin-api-keys', { name: `e106-approver-${RUN}`, description: 'AIGW-106 zweiter Admin', roles: [{ role: 'admin' }] });
  expect(key.status).toBe(201);

  // 1. Bootstrap-Admin lädt über die UI hoch → eingereicht, kein Freigabe-Button für sich selbst.
  await open(page, TOKEN);
  await expect(page.getByTestId('wasm-trust')).toContainText('sigstore-conformance');
  await page.getByTestId('wasm-upload').click();
  const form = page.getByTestId('wasm-form');
  await form.getByTestId('wasm-file').setInputFiles(WASM);
  await form.getByRole('textbox', { name: 'Name' }).fill(mod);
  await page.getByTestId('wasm-form-submit').click();
  await expect(form).toBeHidden();
  const row = page.getByTestId('wasm-pending-row').filter({ hasText: mod });
  await expect(row.getByTestId('wasm-pending-status')).toContainText('wartet auf Freigabe');
  await expect(row.getByTestId('wasm-approve-hint')).toBeVisible();
  await expect(row.getByTestId(`wasm-approve-${mod}`)).toHaveCount(0);
  await page.screenshot({ path: `${process.env.E2E_OUT || './results'}/screens/aigw106-wartet-auf-freigabe.png`, fullPage: true });
  const self = await api(ctx, 'POST', `wasm-modules/${mod}/approve?sha256=${sha}`);
  expect(self.status).toBe(403);
  expect(JSON.stringify(self.body)).toContain('Vier-Augen-Prinzip');
  expect((await api(ctx, 'GET', `wasm-modules/${mod}`)).status).toBe(404);
  clean(page);

  // 2. Zweiter Admin (Admin-Key) gibt über den Button frei → aktiv, Herkunft sichtbar.
  const page2 = await browser.newPage();
  await open(page2, key.body.secret);
  const row2 = page2.getByTestId('wasm-pending-row').filter({ hasText: mod });
  await row2.getByTestId(`wasm-approve-${mod}`).click();
  await expect(page2.getByTestId('wasm-pending-row').filter({ hasText: mod })).toHaveCount(0);
  const active = page2.getByTestId('wasm-row').filter({ hasText: mod });
  await expect(active).toBeVisible();
  const info = await api(ctx, 'GET', `wasm-modules/${mod}`);
  expect(info.status).toBe(200);
  expect(info.body.sha256).toBe(sha);
  expect(info.body.approval).toMatchObject({ uploadedBy: 'bootstrap-admin', approvedBy: `admin-key:e106-approver-${RUN}` });

  // 3. Sigstore-Bundle eines anderen Artefakts (Konformitätssuite, a.txt) → 422.
  const envelope = { content: bytes.toString('base64'), sigstoreBundle: JSON.parse(fs.readFileSync(BUNDLE, 'utf8')) };
  const bad = await ctx.fetch(`/admin/api/v1/wasm-modules/e106-sig-${RUN}?sha256=${sha}`, {
    method: 'PUT',
    headers: { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/json' },
    data: JSON.stringify(envelope),
  });
  expect(bad.status()).toBe(422);
  expect(await bad.text()).toContain('Sigstore');

  // 4. Zweite Einreichung (gleicher Inhalt, neue Beschreibung) verwerfen; aktive Version bleibt.
  const re = await ctx.fetch(`/admin/api/v1/wasm-modules/${mod}?sha256=${sha}&description=neu`, {
    method: 'PUT',
    headers: { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/wasm', 'if-match': info.etag },
    data: bytes,
  });
  expect(re.status()).toBe(202);
  await page2.reload();
  const row3 = page2.getByTestId('wasm-pending-row').filter({ hasText: mod });
  await row3.getByTestId(`wasm-reject-${mod}`).click();
  await page2.getByTestId('wasm-reject-dialog').getByRole('button', { name: 'Verwerfen' }).click();
  await expect(page2.getByTestId('wasm-pending-row').filter({ hasText: mod })).toHaveCount(0);
  expect((await api(ctx, 'GET', `wasm-modules/${mod}`)).body.description).toBeUndefined();
  clean(page2);

  // Audit: submit/approve/reject.
  const audit = await api(ctx, 'GET', `audit-log?object=${mod}&limit=50`);
  const actions = (audit.body.items ?? []).filter((e) => e.kind === 'wasm-module-approval').map((e) => e.action).sort();
  expect(actions).toEqual(['approve', 'reject', 'submit', 'submit']);

  // Aufräumen.
  expect((await api(ctx, 'DELETE', `wasm-modules/${mod}`, undefined, TOKEN, { 'If-Match': '*' })).status).toBe(204);
  await api(ctx, 'DELETE', `admin-api-keys/e106-approver-${RUN}`, undefined, TOKEN, { 'if-match': '*' });
  await page2.close();
});
