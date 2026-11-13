// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-54/55 gegen das echte `aigw serve`: Playground (Chat im Namen eines Keys über die Pipeline, Streaming, sicheres
// Markdown ohne XSS, Kennzahlen, Request-ID mit Link zum Log, Vergleich, Code-Beispiele, Audit) und
// YAML-Import/-Export (Teil-Export, Vorschau, Fehler mit Zeilenbezug, Trockenlauf mit Diff, Anwenden mit
// Bestätigung, YAML-Link auf Detailseiten). Jeweils ohne CSP-Verstöße.
import { api, clean, ensure, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const ORG = 'e54-org';
const TEAM = 'e54-team';
const KEY = 'e54-playground';
const GROUP = 'e54-echo';
const OTHER = 'e54-other';

test.beforeAll(async ({ request }) => {
  await ensure(request, 'organizations', { metadata: { name: ORG }, spec: { displayName: 'AIGW-54 Org' } });
  await ensure(request, 'teams', { metadata: { name: TEAM }, spec: { organization: ORG } });
  for (const g of [GROUP, OTHER]) {
    await ensure(request, 'model-groups', { metadata: { name: g }, spec: { routing: 'weighted' } });
    await ensure(request, 'deployments', { metadata: { name: `${g}-mock` }, spec: { modelGroup: g, provider: 'mock', providerModel: 'mock', enabled: true } });
  }
  await ensure(request, 'prices', { spec: { provider: 'mock', model: 'mock', validFrom: '2025-01-01T00:00:00Z', perMillionTokens: { input: '1000', output: '2000' } } });
  await ensure(request, 'api-keys', { metadata: { name: KEY }, spec: { organization: ORG, team: TEAM, allowedModels: [GROUP, OTHER] } });
  // Der neue Key gilt erst nach dem Hot Reload.
  await expect.poll(async () => (await api(request, 'GET', `playground/models?key=${KEY}`)).status, { timeout: 15_000 }).toBe(200);
});

test('Playground: Streaming-Chat, sicheres Markdown, Kennzahlen, Log-Link, Audit', async ({ page, request }) => {
  await useToken(page);
  await gotoReal(page, `/playground?key=${KEY}&model=${GROUP}`);
  await expect(page.getByTestId('pg-pane')).toHaveCount(1);
  await page.getByTestId('pg-system').fill('Antworte knapp.');
  await page.getByTestId('pg-temperature').fill('0.2');
  await page.getByTestId('pg-max-tokens').fill('64');
  // Markdown mit XSS-Versuchen: das Echo des Mock-Anbieters kommt als Antwort zurück.
  await page.getByTestId('pg-prompt').fill('**fett** <img src=x onerror="window.__xss=1"> [böse](javascript:alert(1)) [gut](https://example.org)');
  await page.getByTestId('pg-send').click();
  const answer = page.getByTestId('pg-answer').first();
  await expect(answer).toContainText('Echo:');
  await expect(answer.locator('strong')).toHaveText('fett');
  await expect(answer).toContainText('<img src=x');
  await expect(answer.locator('img')).toHaveCount(0);
  await expect(answer.locator('a')).toHaveCount(1);
  await expect(answer.locator('a')).toHaveAttribute('href', 'https://example.org/');
  await expect(answer.locator('a')).toHaveAttribute('rel', /noopener/);
  expect(await page.evaluate(() => window.__xss)).toBeUndefined();

  const metrics = page.getByTestId('pg-metrics').first();
  await expect(metrics.getByTestId('pg-deployment')).toHaveText(`${GROUP}-mock`);
  await expect(metrics.getByTestId('pg-ttft')).toBeVisible();
  await expect(metrics.getByTestId('pg-latency')).toBeVisible();
  await expect(metrics.getByTestId('pg-tokens')).toBeVisible();
  // Kosten aus dem Request-Log (asynchron geschrieben).
  await expect(metrics.getByTestId('pg-cost')).toBeVisible({ timeout: 15_000 });
  const rid = (await metrics.getByTestId('pg-request-link').textContent()).trim();
  expect(rid).toMatch(/^[0-9a-f-]{36}$/);

  // Audit-Eintrag mit Key und Request-ID, ohne Inhalte.
  const audit = await api(request, 'GET', 'audit-log?kind=playground&limit=50');
  expect(audit.status).toBe(200);
  const entry = audit.body.items.find((e) => e.after?.requestId === rid);
  expect(entry, JSON.stringify(audit.body)).toBeTruthy();
  expect(entry.objectName).toBe(KEY);
  expect(entry.team).toBe(TEAM);
  expect(JSON.stringify(entry)).not.toContain('fett');

  await page.screenshot({ path: `${OUT}/screens/echt-playground-hell-desktop.png` });

  // Link zum Log-Explorer öffnet das Detail der Anfrage.
  await metrics.getByTestId('pg-request-link').click();
  await expect(page).toHaveURL(new RegExp(`/admin/logs\\?requestId=${rid}`));
  await expect(page.getByTestId('request-detail')).toContainText(rid);
  clean(page);
});

test('Playground: Vergleich zweier Modelle, JSON-Modus mit n=2, Code-Beispiele', async ({ page }) => {
  await useToken(page);
  await gotoReal(page, `/playground?key=${KEY}&model=${GROUP}`);
  await page.getByTestId('pg-add-pane').click();
  const panes = page.getByTestId('pg-pane');
  await expect(panes).toHaveCount(2);
  // Streaming aus (JSON) und zwei Varianten.
  await page.getByTestId('pg-stream').click();
  await page.getByTestId('pg-params').getByRole('combobox').nth(1).click();
  await page.getByRole('option', { name: '2', exact: true }).click();
  await page.getByTestId('pg-prompt').fill('Hallo Vergleich');
  await page.getByTestId('pg-prompt').press('Control+Enter');
  for (const i of [0, 1]) {
    await expect(panes.nth(i).getByTestId('pg-answer')).toHaveCount(2);
    await expect(panes.nth(i).getByTestId('pg-answer').first()).toContainText('Echo: Hallo Vergleich');
  }
  await expect(panes.nth(0).getByTestId('pg-answer').nth(1)).toContainText('Variante 2');
  await expect(panes.nth(0).getByTestId('pg-deployment')).toHaveText(`${GROUP}-mock`);
  await expect(panes.nth(1).getByTestId('pg-deployment')).toHaveText(`${OTHER}-mock`);

  // Code-Beispiele zur aktuellen Anfrage.
  await page.getByTestId('pg-code').click();
  const sheet = page.getByTestId('pg-snippets');
  await expect(sheet).toContainText('/v1/chat/completions');
  await expect(sheet).toContainText('AIGW_API_KEY');
  await page.getByTestId('pg-snippet-openai').click();
  await expect(sheet).toContainText('from openai import OpenAI');
  await page.getByTestId('pg-snippet-anthropic').click();
  await expect(sheet).toContainText('anthropic.Anthropic');
  await page.keyboard.press('Escape');
  clean(page);
});

test('Playground-API: Key nötig, fremde/unbekannte Keys 404, Freigaben der Datenebene gelten', async ({ request }) => {
  const r = await api(request, 'GET', 'playground/models?key=gibt-es-nicht');
  expect(r.status).toBe(404);
  const res = await request.fetch(`/admin/api/v1/playground/chat?key=${KEY}`, {
    method: 'POST',
    headers: { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/json' },
    data: JSON.stringify({ model: 'gibt-es-nicht', messages: [{ role: 'user', content: 'x' }] }),
  });
  expect(res.status()).toBe(404);
  expect((await res.json()).error.code).toBe('model_not_found');
  const anon = await request.fetch(`/admin/api/v1/playground/chat?key=${KEY}`, { method: 'POST', data: '{}' });
  expect(anon.status()).toBe(401);
});

test('YAML-Export: gesamt, Teil-Export per Detailseite, keine Geheimwerte', async ({ page, request }) => {
  await ensure(request, 'provider-credentials', { metadata: { name: 'e55-cred' }, spec: { provider: 'openai', data: { apiKey: { value: 'sk-geheim-e55-nicht-exportieren' } } } });
  await useToken(page);
  await gotoReal(page, '/config');
  await page.getByTestId('gitops-preview').click();
  const preview = page.getByTestId('gitops-export-preview');
  await expect(preview).toContainText('apiVersion: aigw/v1alpha1');
  await expect(preview).toContainText('e55-cred');
  await expect(preview).toContainText('AIGW_SECRET_E55_CRED_API_KEY');
  await expect(preview).not.toContainText('sk-geheim-e55');

  // Download
  const [dl] = await Promise.all([page.waitForEvent('download'), page.getByTestId('gitops-export').click()]);
  expect(dl.suggestedFilename()).toMatch(/^aigw-config-gesamt-\d{4}-\d{2}-\d{2}\.yaml$/);

  // Teil-Export aus der Team-Detailseite.
  await gotoReal(page, `/teams/${TEAM}`);
  await page.getByTestId('detail-yaml').click();
  await expect(page).toHaveURL(/\/admin\/config\?team=/);
  await expect(preview).toContainText(`name: ${TEAM}`);
  await expect(preview).toContainText(`name: ${KEY}`);
  await expect(preview).not.toContainText('kind: ModelGroup');
  await expect(preview).toContainText('Teil-Export');
  clean(page);
});

test('YAML-Import: Fehler mit Zeilenbezug, Trockenlauf mit Diff, Anwenden mit Bestätigung', async ({ page, request }) => {
  await useToken(page);
  await gotoReal(page, '/config');
  const editor = page.getByTestId('gitops-yaml');
  // Dokument 2 ab Zeile 5 hat ein unbekanntes Feld.
  await editor.fill(
    [
      'apiVersion: aigw/v1alpha1',
      'kind: ModelGroup',
      'metadata: {name: e55-import}',
      '---',
      'apiVersion: aigw/v1alpha1',
      'kind: Team',
      'metadata: {name: e55-team}',
      `spec: {organization: ${ORG}, bogus: 1}`,
    ].join('\n'),
  );
  await page.getByTestId('gitops-plan').click();
  const issues = page.getByTestId('gitops-issues');
  await expect(issues).toContainText('Zeile 5');
  await expect(issues).toContainText('bogus');
  await expect(page.getByTestId('yaml-editor').locator('[data-error]')).toHaveText('5');
  await page.getByTestId('gitops-issue-line').first().click();
  expect(await editor.evaluate((el) => el.value.slice(el.selectionStart, el.selectionEnd))).toBe('apiVersion: aigw/v1alpha1');
  await expect(page.getByTestId('gitops-apply')).toBeDisabled();

  // Korrigieren → Trockenlauf mit Diff → Anwenden nach Bestätigung.
  await editor.fill(
    [
      'apiVersion: aigw/v1alpha1',
      'kind: ModelGroup',
      'metadata: {name: e55-import}',
      'spec: {routing: weighted, aliases: [e55-alias]}',
      '---',
      'apiVersion: aigw/v1alpha1',
      'kind: Team',
      'metadata: {name: e55-team}',
      `spec: {organization: ${ORG}}`,
    ].join('\n'),
  );
  await page.getByTestId('gitops-plan').click();
  const plan = page.getByTestId('apply-plan');
  await expect(plan).toContainText('e55-import');
  await expect(page.getByTestId('gitops-issues')).toHaveCount(0);
  await page.getByTestId('gitops-apply').click();
  const confirm = page.getByTestId('gitops-confirm');
  await expect(confirm).toContainText('Konfiguration anwenden?');
  await confirm.getByRole('button', { name: 'Anwenden' }).click();
  await expect(confirm).toHaveCount(0);
  await expect(plan).toContainText('→');
  const g = await api(request, 'GET', 'model-groups/e55-import');
  expect(g.status).toBe(200);
  expect(g.body.spec.aliases).toEqual(['e55-alias']);
  await page.screenshot({ path: `${OUT}/screens/echt-yaml-import-hell-desktop.png`, fullPage: true });
  clean(page, { allow: [/Failed to load resource.*422/] });
});
