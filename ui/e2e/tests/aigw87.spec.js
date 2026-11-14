// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-87 gegen das echte `aigw serve`: Tool-Inventar aus Anfragen der Datenebene (Mock-Deployment),
// Seite „Tools“ mit Filter, Detail-Sheet (Beschreibung, Hash-Verlauf), Sperren je Team (Richtlinie
// tools-block-team-…, Wirkung 403 auf der Datenebene), Freigeben, Richtlinien-Reiter (Anlegen, Löschen).
// Ohne CSP-Verstöße und Konsolenfehler.
import { api, clean, ensure, expect, gotoReal, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const ORG = 'e87-org';
const TEAM = 'e87-team';
const GROUP = 'e87-model';
let secret = '';

function chat(request, tools) {
  return request.fetch('/v1/chat/completions', {
    method: 'POST',
    headers: { authorization: `Bearer ${secret}`, 'content-type': 'application/json', 'x-aigw-client': 'e2e-app/1.0' },
    data: JSON.stringify({
      model: GROUP,
      messages: [{ role: 'user', content: 'tool bitte' }],
      tools: tools.map((name) => ({ type: 'function', function: { name, description: `Beschreibung von ${name}`, parameters: { type: 'object', properties: { q: { type: 'string' } } } } })),
    }),
  });
}

test.beforeAll(async ({ request }) => {
  await ensure(request, 'organizations', { metadata: { name: ORG }, spec: {} });
  await ensure(request, 'teams', { metadata: { name: TEAM }, spec: { organization: ORG } });
  await ensure(request, 'model-groups', { metadata: { name: GROUP }, spec: {} });
  await ensure(request, 'deployments', { metadata: { name: `${GROUP}-mock` }, spec: { modelGroup: GROUP, provider: 'mock', providerModel: 'mock', enabled: true } });
  const k = await api(request, 'POST', 'api-keys', { metadata: { name: `e87-key-${Date.now()}` }, spec: { organization: ORG, team: TEAM, payloadMode: 'full' } });
  expect(k.status).toBe(201);
  secret = k.body.secret;
  // Hot Reload abwarten, dann Tools schicken.
  await expect.poll(async () => (await chat(request, ['mcp__e87srv__search', 'e87_calc'])).status(), { timeout: 15_000 }).toBe(200);
  // Inventar wird gepuffert geschrieben (tools.flush_interval_ms, Standard 5 s).
  await expect
    .poll(async () => ((await api(request, 'GET', 'tools?q=e87')).body?.items ?? []).length, { timeout: 20_000 })
    .toBe(2);
});

test('Tools: Inventar, Detail, Sperren je Team, Freigeben', async ({ page, request }) => {
  await useToken(page);
  await gotoReal(page, '/tools');
  const table = page.getByTestId('tools-table');
  await expect(table).toContainText('mcp__e87srv__search');
  await expect(table).toContainText('MCP e87srv');
  await expect(table).toContainText('e2e-app/1.0');
  // Filter Herkunft.
  await page.getByLabel('Herkunft (client oder MCP-Server)').fill('e87srv');
  await expect(table).not.toContainText('e87_calc');
  await page.getByLabel('Herkunft (client oder MCP-Server)').fill('');
  await expect(table).toContainText('e87_calc');

  // Detail: Beschreibung (payloadMode full) und Verlauf.
  await table.getByTestId('tool-name').filter({ hasText: 'e87_calc' }).click();
  const sheet = page.getByTestId('tool-detail');
  await expect(sheet.getByTestId('tool-description')).toHaveText('Beschreibung von e87_calc');
  await expect(sheet.getByTestId('tool-history')).toContainText('erstmals gesehen');
  await page.keyboard.press('Escape');

  // Sperren auf Team-Ebene mit Ablehnung.
  const row = table.locator('tr', { hasText: 'e87_calc' });
  await row.getByRole('button', { name: /Aktionen für e87_calc/ }).click();
  await page.getByRole('menuitem', { name: /Sperren/ }).click();
  const form = page.getByTestId('tool-decision-form');
  await form.getByLabel('Ebene').click();
  await page.getByRole('option', { name: `Team ${TEAM}` }).click();
  await form.getByLabel('Wirkung').click();
  await page.getByRole('option', { name: /ablehnen/ }).click();
  await form.getByRole('button', { name: 'Sperren' }).click();
  await expect(row).toContainText('gesperrt');
  await expect(row).toContainText(`tools-block-team-${TEAM}`);
  await expect.poll(async () => (await chat(request, ['e87_calc'])).status(), { timeout: 15_000 }).toBe(403);

  // Freigeben auf derselben Ebene hebt die Sperre auf.
  await row.getByRole('button', { name: /Aktionen für e87_calc/ }).click();
  await page.getByRole('menuitem', { name: /Freigeben/ }).click();
  await form.getByLabel('Ebene').click();
  await page.getByRole('option', { name: `Team ${TEAM}` }).click();
  await form.getByRole('button', { name: 'Freigeben' }).click();
  await expect(row).toContainText('freigegeben');
  await expect.poll(async () => (await chat(request, ['e87_calc'])).status(), { timeout: 15_000 }).toBe(200);
  clean(page);
});

test('Tools: Richtlinien anlegen und löschen', async ({ page }) => {
  await useToken(page);
  await gotoReal(page, '/tools');
  await page.getByTestId('tab-policies').click();
  const table = page.getByTestId('tool-policies-table');
  await expect(table).toContainText(`tools-allow-team-${TEAM}`);
  await page.getByTestId('create-tool-policy').click();
  const form = page.getByTestId('tool-policy-form');
  const name = `e87-pol-${Date.now()}`;
  await form.getByLabel('Name').fill(name);
  await form.getByLabel('Tool-Muster').fill('*delete*\n!delete_draft');
  await form.getByRole('button', { name: 'Anlegen' }).click();
  const row = table.locator('tr', { hasText: name });
  await expect(row).toContainText('block-strip');
  await expect(row).toContainText('!delete_draft');
  await row.getByRole('button', { name: new RegExp(`Aktionen für ${name}`) }).click();
  await page.getByRole('menuitem', { name: /Löschen/ }).click();
  await page.getByRole('button', { name: 'Löschen' }).click();
  await expect(table).not.toContainText(name);
  clean(page);
});
