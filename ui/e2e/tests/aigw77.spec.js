// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-77 gegen das echte `aigw serve`: Detailseiten (D8) mit Audit und Nutzung, strukturierte Routing-Formulare
// (D9), Limit-Editor/Payload-Modus, serverseitige Pagination (D11), Banner (Snapshot F15, Bootstrap/Nur-YAML F56,
// admin.ui.mock F135), Key-Zustände (F37), Log-Explorer mit Trace-Link, Audit-Seite (Export, Kettenprüfung),
// Einladungen (F90, zweiter Server mit OIDC). Jeweils ohne CSP-Verstöße.
import { api, clean, ensure, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const ORG = 'e77-org';
const TEAM = 'e77-team';
let SECRET = '';
// Je Lauf eigener Key (der Klartext ist nur bei der Anlage bekannt).
const KEY = `e77-traffic-${Date.now().toString(36)}`;

test.beforeAll(async ({ request }) => {
  await ensure(request, 'organizations', { metadata: { name: ORG }, spec: { displayName: 'AIGW-77 Org', payloadMode: 'metadata' } });
  await ensure(request, 'teams', { metadata: { name: TEAM }, spec: { organization: ORG, displayName: 'AIGW-77 Team' } });
  await ensure(request, 'model-groups', { metadata: { name: 'e77-echo' }, spec: { routing: 'weighted', settings: { retry: { maxAttempts: 2 }, vendorExtra: 1 } } });
  await ensure(request, 'deployments', { metadata: { name: 'e77-echo-mock' }, spec: { modelGroup: 'e77-echo', provider: 'mock', providerModel: 'mock', enabled: true } });
  await ensure(request, 'prices', { spec: { provider: 'mock', model: 'mock', validFrom: '2025-01-01T00:00:00Z', perMillionTokens: { input: '1000', output: '2000' } } });
  // Key mit Klartext für den Datenverkehr (vorhandenen neu anlegen, damit der Klartext bekannt ist).
  const k = await api(request, 'POST', 'api-keys', { metadata: { name: KEY }, spec: { organization: ORG, team: TEAM, budget: { amount: '100', period: 'month' } } });
  expect(k.status).toBe(201);
  SECRET = k.body.secret;
  // Etwas Verkehr: Anfragen über die Datenebene (gleicher Port), damit Analytics/Logs Daten haben. Der neue Key gilt
  // erst nach dem Hot Reload – bis dahin 401.
  const chat = (i) =>
    request.post('/v1/chat/completions', {
      headers: { authorization: `Bearer ${SECRET}`, 'content-type': 'application/json', 'x-request-id': `e77-client-${i}` },
      data: JSON.stringify({ model: 'e77-echo', messages: [{ role: 'user', content: `Hallo ${i}` }] }),
    });
  await expect.poll(async () => (await chat(0)).status(), { timeout: 15_000 }).toBe(200);
  for (let i = 1; i < 6; i++) expect((await chat(i)).status()).toBe(200);
  // Warten, bis der Writer Log und Rollups geschrieben hat.
  await expect
    .poll(async () => (await api(request, 'GET', `request-logs?key=${KEY}&limit=10`)).body?.items?.length ?? 0, { timeout: 20_000 })
    .toBeGreaterThanOrEqual(6);
});

test('Detailseiten: Key, Team, Organisation, Modellgruppe, Deployment mit Audit und Nutzung', async ({ page }) => {
  await useToken(page);
  await gotoReal(page, `/keys?org=${ORG}`);
  await page.getByTestId('keys-table').getByRole('link', { name: KEY }).click();
  const key = page.getByTestId('key-detail');
  await expect(key).toBeVisible();
  await expect(key.getByRole('heading', { level: 1 })).toHaveText(KEY);
  await expect(page.getByTestId('budgets')).toContainText('100');
  // Payload-Modus geerbt von der Organisation (F30)
  await page.getByTestId('tab-usage').click();
  await expect(page.getByTestId('usage-kpis')).toContainText('Requests');
  await expect(page.getByTestId('stacked-area').locator('path').first()).toBeAttached();
  await page.getByTestId('tab-audit').click();
  await expect(page.getByTestId('audit-entry').first()).toContainText('angelegt');
  expect(page.url()).toContain('?tab=audit');
  await page.getByTestId('audit-entry').first().getByRole('button').click();

  await gotoReal(page, `/teams/${TEAM}`);
  await expect(page.getByTestId('team-detail')).toBeVisible();
  await expect(page.getByTestId('eff-payload')).toContainText(`geerbt von Organisation ${ORG}`);
  await expect(page.getByTestId('team-keys')).toContainText(KEY);
  await page.getByTestId('team-detail').getByRole('link', { name: ORG }).first().click();
  await expect(page.getByTestId('org-detail')).toBeVisible();
  await expect(page.getByTestId('org-teams')).toContainText('AIGW-77 Team');

  await gotoReal(page, '/models');
  await page.getByTestId('group-list').getByRole('link', { name: 'e77-echo', exact: true }).click();
  await expect(page.getByTestId('group-detail')).toBeVisible();
  await expect(page.getByTestId('settings-view')).toContainText('Versuche');
  await expect(page.getByTestId('settings-view')).toContainText('vendorExtra');
  await expect(page.getByTestId('deployments-table')).toContainText('Mock-Anbieter');
  await page.getByRole('link', { name: 'e77-echo-mock' }).click();
  await expect(page.getByTestId('deployment-detail')).toBeVisible();
  await expect(page.getByTestId('mock-provider-hint')).toBeVisible();
  await page.screenshot({ path: `${OUT}/screens/echt-deployment-detail-hell-desktop.png`, fullPage: true });
  clean(page);
});

test('Strukturierte Routing-Einstellungen (D9) mit JSON-Ansicht, Limits mit Kalenderfenster, Payload-Modus', async ({ page, request }) => {
  await useToken(page);
  await gotoReal(page, '/models/e77-echo');
  await page.getByTestId('edit-group').click();
  const dlg = page.getByTestId('group-form-dialog');
  const attempts = dlg.getByRole('textbox', { name: 'Versuche' });
  await expect(attempts).toHaveValue('2');
  await attempts.fill('99');
  await expect(dlg.getByText('Ungültige Zahl')).toBeVisible();
  await attempts.fill('4');
  await dlg.getByRole('textbox', { name: 'Fehlerquote (0–1]' }).fill('0.25');
  await dlg.getByRole('combobox', { name: 'Modellname in der Antwort' }).click();
  await page.getByRole('option', { name: 'angefragter Name (Gruppe/Alias)' }).click();
  await dlg.getByRole('textbox', { name: 'Gesamt (ms)' }).fill('60000');
  await page.getByTestId('settings-json-tab').click();
  await expect(page.getByTestId('settings-json')).toHaveValue(/"maxAttempts": 4/);
  await expect(page.getByTestId('settings-json')).toHaveValue(/"vendorExtra": 1/);
  await page.getByTestId('settings-form-tab').click();
  await page.screenshot({ path: `${OUT}/screens/echt-routing-einstellungen-hell-desktop.png` });
  await page.getByTestId('group-form-submit').click();
  await expect(dlg).toBeHidden();
  const g = await api(request, 'GET', 'model-groups/e77-echo');
  expect(g.body.spec.settings).toMatchObject({ retry: { maxAttempts: 4 }, circuitBreaker: { failureRate: 0.25 }, responseModel: 'requested', timeouts: { totalMs: 60000 }, vendorExtra: 1 });

  // Team: Limit mit Kalendermonat und eigener Payload-Modus.
  await gotoReal(page, `/teams/${TEAM}`);
  await page.getByTestId('edit-entity').click();
  const form = page.getByTestId('entity-form');
  await form.getByTestId('add-limit').click();
  await form.getByRole('textbox', { name: 'Grenze 1' }).fill('5000');
  await form.getByRole('combobox', { name: 'Fenster 1' }).click();
  await page.getByRole('option', { name: 'Kalendermonat' }).click();
  await form.getByRole('combobox', { name: 'Payload-Logging' }).click();
  await page.getByRole('option', { name: 'maskiert' }).click();
  await page.getByTestId('entity-form-submit').click();
  await expect(form).toBeHidden();
  const t = await api(request, 'GET', `teams/${TEAM}`);
  expect(t.body.spec.limits).toContainEqual({ metric: 'requests', window: 'calendar-month', limit: 5000 });
  expect(t.body.spec.payloadMode).toBe('masked');
  await expect(page.getByTestId('eff-payload')).toContainText('eigener Wert');
  clean(page);
});

test('Serverseitige Pagination und Suche (D11)', async ({ page, request }) => {
  for (let i = 0; i < 28; i++) await ensure(request, 'organizations', { metadata: { name: `e77-pg-${String(i).padStart(2, '0')}` }, spec: {} });
  await useToken(page);
  const pages = [];
  page.on('request', (r) => r.url().includes('/admin/api/v1/organizations?') && pages.push(r.url()));
  await gotoReal(page, '/teams?tab=org');
  const table = page.getByTestId('orgs-table');
  await expect(table.locator('tbody tr')).toHaveCount(25);
  const pager = page.getByTestId('orgs-table-pager');
  await expect(pager.getByTestId('page-next')).toBeEnabled();
  await pager.getByTestId('page-next').click();
  await expect(table).toContainText('Seite 2');
  await expect(pager.getByTestId('page-prev')).toBeEnabled();
  expect(pages.some((u) => u.includes('cursor='))).toBe(true);
  await table.getByRole('searchbox').fill('e77-pg-07');
  await expect(table.locator('tbody tr')).toHaveCount(1);
  expect(pages.some((u) => u.includes('q=e77-pg-07'))).toBe(true);
  clean(page);
});

test('Banner: Snapshot veraltet, Bootstrap aktiv, Nur-YAML-Modus, Mock-Schalter ausgeblendet', async ({ page }) => {
  await useToken(page);
  // Bootstrap-Token ist im Testserver aktiv (kein globaler Admin angelegt).
  await gotoReal(page, '/');
  await expect(page.getByTestId('bootstrap-banner')).toBeVisible();
  await expect(page.getByTestId('snapshot-banner')).toHaveCount(0);
  // F15: Snapshot-Zustand aus /version (hier simuliert).
  await page.route('**/admin/api/v1/version', async (route) => {
    const res = await route.fetch();
    const body = await res.json();
    await route.fulfill({ response: res, json: { ...body, snapshot: { version: 7, ageSeconds: 900, source: 'database', stale: true, failing: true } } });
  });
  // F56/F135: Nur-YAML-Modus und admin.ui.mock: false (hier simuliert).
  await page.route('**/admin/api/v1/auth/info', async (route) => {
    const res = await route.fetch();
    const body = await res.json();
    await route.fulfill({ response: res, json: { ...body, readOnly: true, uiMock: false } });
  });
  await page.goto('/admin/keys?mock=1');
  await expect(page.getByTestId('snapshot-banner')).toContainText('veraltet');
  await expect(page.getByTestId('readonly-banner')).toBeVisible();
  await expect(page.getByTestId('mock-banner')).toHaveCount(0);
  await expect(page.getByTestId('keys-table')).toBeVisible();
  await expect(page.getByTestId('create-key')).toHaveCount(0);
  await page.screenshot({ path: `${OUT}/screens/echt-banner-hell-desktop.png` });
  await page.goto('/admin/settings');
  await expect(page.getByTestId('session-card')).toBeVisible();
  await expect(page.getByTestId('mock-switch')).toHaveCount(0);
  clean(page);
});

test('Key-Zustände: sperren, entsperren, endgültig widerrufen (F37), Statusfilter', async ({ page, request }) => {
  await ensure(request, 'api-keys', { metadata: { name: 'e77-state' }, spec: { organization: ORG, team: TEAM } });
  await useToken(page);
  await gotoReal(page, '/keys/e77-state');
  const detail = page.getByTestId('key-detail');
  await detail.getByTestId('key-suspend').click();
  await page.getByTestId('key-action-dialog').getByTestId('confirm-action').click();
  await expect(detail).toContainText('Gesperrt');
  expect((await api(request, 'GET', 'api-keys/e77-state')).body.status.state).toBe('suspended');
  await detail.getByTestId('key-resume').click();
  await page.getByTestId('key-action-dialog').getByTestId('confirm-action').click();
  await expect(detail.getByTestId('key-suspend')).toBeVisible();
  // Endgültig: erst nach Eingabe des Namens.
  await detail.getByTestId('key-revoke').click();
  const dlg = page.getByTestId('key-action-dialog');
  await expect(dlg.getByTestId('confirm-action')).toBeDisabled();
  await dlg.getByTestId('confirm-text').fill('e77-state');
  await dlg.getByTestId('confirm-action').click();
  await expect(detail).toContainText('Widerrufen');
  await expect(detail.getByTestId('key-resume')).toHaveCount(0);
  expect((await api(request, 'GET', 'api-keys/e77-state')).body.status.state).toBe('revoked');
  // Filter (serverseitig) zeigt den Key unter „Widerrufen“.
  await gotoReal(page, `/keys?state=revoked&org=${ORG}`);
  await expect(page.getByTestId('keys-table')).toContainText('e77-state');
  await expect(page.getByTestId('keys-table')).not.toContainText(KEY);
  clean(page);
});

test('Übersicht und Log-Explorer mit echten Daten, Trace-Link, Audit-Seite mit Export und Kettenprüfung', async ({ page }) => {
  await useToken(page);
  await gotoReal(page, `/?org=${ORG}`);
  await expect(page.getByTestId('kpis')).toContainText('Requests');
  await expect(page.getByTestId('top-models')).toContainText('e77-echo');
  await expect(page.getByTestId('cost-treemap').locator('rect').first()).toBeAttached();
  await page.screenshot({ path: `${OUT}/screens/echt-uebersicht-hell-desktop.png`, fullPage: true });

  await gotoReal(page, `/logs?key=${KEY}&org=${ORG}`);
  const table = page.getByTestId('logs-table');
  await expect(table.locator('tbody tr').first()).toContainText('e77-echo');
  await table.locator('tbody tr').first().click();
  const sheet = page.getByTestId('request-detail');
  await expect(sheet.getByTestId('attempts')).toContainText('e77-echo-mock');
  const trace = sheet.getByTestId('trace-link');
  if (await trace.count()) await expect(trace).toHaveAttribute('href', /^https:\/\/tracing\.example\.org\/trace\//);
  await page.keyboard.press('Escape');
  // Filter Cache-Embedding (F191) wird an den Server gegeben.
  await page.getByRole('combobox', { name: 'Cache' }).click();
  await page.getByRole('option', { name: 'Cache-Embedding' }).click();
  await expect(page).toHaveURL(/cache=cache-embedding/);

  await gotoReal(page, '/audit?kind=api-key');
  await expect(page.getByTestId('audit-entry').first()).toBeVisible();
  const [dl] = await Promise.all([page.waitForEvent('download'), page.getByTestId('audit-export').click()]);
  expect(dl.suggestedFilename()).toMatch(/\.ndjson$/);
  await page.getByTestId('audit-verify').click();
  await expect(page.getByTestId('audit-verify-result')).toContainText('unversehrt');
  await page.screenshot({ path: `${OUT}/screens/echt-audit-hell-desktop.png` });
  clean(page);
});

test('Einladung (F90): Link erzeugen, anzeigen, widerrufen (Server mit OIDC)', async ({ page, playwright }) => {
  const OIDC = process.env.AIGW_OIDC_URL;
  test.skip(!OIDC, 'AIGW_OIDC_URL fehlt (run.sh startet den zweiten Server)');
  const ctx = await playwright.request.newContext({ baseURL: OIDC });
  await ensure(ctx, 'users', { metadata: { name: 'e77-invitee' }, spec: { displayName: 'Ina Eingeladen', oidcIssuer: null, roles: [{ role: 'viewer' }] } });
  await useToken(page);
  await page.goto(`${OIDC}/admin/teams?tab=user&theme=light`);
  await page.getByTestId('users-table').getByText('Ina Eingeladen').click();
  const inv = page.getByTestId('invitation');
  await expect(inv).toContainText('Keine offene Einladung');
  await inv.getByTestId('invitation-create').click();
  await expect(inv.getByTestId('invitation-url')).toContainText('/admin/auth/login?invite=');
  await expect(inv).toContainText('offen');
  await inv.getByTestId('invitation-revoke').click();
  await expect(inv).toContainText('Keine offene Einladung');
  const r = await api(ctx, 'GET', 'users/e77-invitee/invitation');
  expect(r.status).toBe(404);
  // OIDC-Kennung vorab im Formular (F90).
  await page.getByTestId('detail-sheet').getByRole('button', { name: 'Bearbeiten' }).click();
  await expect(page.getByTestId('entity-form').getByRole('textbox', { name: /OIDC-Kennung/ })).toBeVisible();
  await ctx.dispose();
  // GET …/invitation ohne offene Einladung antwortet absichtlich 404.
  clean(page, { allow: [/status of 404/] });
});
