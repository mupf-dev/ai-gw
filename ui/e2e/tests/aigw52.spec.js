// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// AIGW-52 gegen das echte `aigw serve`: Editoren für Plugin-Bindungen (strukturiertes PII-Formular, JSON-Ansicht,
// Testlauf mit Vorschau, Backend-Fehler am Feld), Kettenvorschau (Zwiebelprinzip, override/disable) und MCP-Server
// samt Freigaben (Secret nur als Verweis, ETag-Konflikt). Jeweils ohne CSP-Verstöße.
import { api, clean, ensure, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const ORG = 'e52-org';
const TEAM = 'e52-team';
const RUN = Date.now().toString(36);

test.beforeAll(async ({ request }) => {
  await ensure(request, 'organizations', { metadata: { name: ORG }, spec: { displayName: 'AIGW-52 Org' } });
  await ensure(request, 'teams', { metadata: { name: TEAM }, spec: { organization: ORG, displayName: 'AIGW-52 Team' } });
});

async function pick(page, scope, name, option) {
  await scope.getByRole('combobox', { name, exact: true }).click();
  await page.getByRole('option', { name: option, exact: true }).click();
}

test('Guardrails: PII-Formular, Testlauf mit Vorschau, JSON-Ansicht, Backend-Fehler am Feld', async ({ page, request }) => {
  const name = `e52-pii-${RUN}`;
  await useToken(page);
  await gotoReal(page, '/guardrails');
  await page.getByTestId('create-binding').click();
  const dlg = page.getByTestId('binding-form');
  await dlg.getByRole('textbox', { name: 'Name' }).fill(name);
  await pick(page, dlg, 'Aktion (Standard aller Kategorien)', 'mask – Platzhalter [EMAIL_1]');
  // Auswahl je Kategorie: Kreditkarte ablehnen, IP-Adressen aus, Rest mit der Standardaktion.
  await pick(page, dlg, 'Kategorien', 'Auswahl je Kategorie');
  await pick(page, dlg, 'Aktion Kreditkarte', 'reject – ablehnen');
  await pick(page, dlg, 'Aktion IP-Adresse', 'aus');
  await dlg.getByRole('textbox', { name: 'Ausnahmen (Allowlist)' }).fill('support@acme.example');

  // Testlauf mit der ungespeicherten Konfiguration: E-Mail wird maskiert, Treffer gezählt.
  await page.getByTestId('plugin-test-input').fill('Bitte an max@example.org schreiben');
  await page.getByTestId('plugin-test-run').click();
  const out = page.getByTestId('plugin-test-output');
  await expect(out).toBeVisible();
  await expect(out).not.toContainText('max@example.org');
  await expect(page.getByTestId('plugin-test-result')).toContainText('verändert');
  await expect(page.getByTestId('plugin-test-result')).toContainText('email · mask');
  // Ausnahme greift.
  await page.getByTestId('plugin-test-input').fill('support@acme.example');
  await page.getByTestId('plugin-test-run').click();
  await expect(page.getByTestId('plugin-test-result')).toContainText('unverändert');
  await page.screenshot({ path: `${OUT}/screens/echt-guardrails-pii-hell-desktop.png` });

  // JSON-Ansicht zeigt dieselbe Konfiguration.
  await page.getByTestId('config-mode').click();
  await expect(page.getByTestId('config-json')).toHaveValue(/"creditCard": "reject"/);
  await page.getByTestId('config-mode').click();

  // Backend-Fehler am Feld: unbekanntes Team (scope.target), dann zu großes Zeitlimit (Registry-Prüfung).
  await pick(page, dlg, 'Ebene', 'Team');
  await dlg.getByRole('combobox', { name: 'Ziel' }).fill('gibt-es-nicht');
  await page.getByTestId('binding-form-submit').click();
  await expect(dlg.getByText(/unbekanntes Team/)).toBeVisible();
  await pick(page, dlg, 'Ebene', 'global');
  await dlg.getByRole('textbox', { name: 'Zeitlimit (ms)' }).fill('999999');
  await page.getByTestId('binding-form-submit').click();
  await expect(dlg.getByText(/timeoutMs höchstens/)).toBeVisible();
  await dlg.getByRole('textbox', { name: 'Zeitlimit (ms)' }).fill('');
  await page.getByTestId('binding-form-submit').click();
  await expect(dlg).toBeHidden();

  const b = await api(request, 'GET', `plugin-bindings/${name}`);
  expect(b.status).toBe(200);
  expect(b.body.spec.config).toMatchObject({ action: 'mask', categories: { creditCard: 'reject', email: {} }, allow: ['support@acme.example'] });
  expect(b.body.spec.config.categories.ipAddress).toBeUndefined();
  expect(b.body.effective.failMode).toBe('closed');

  // Bearbeiten lädt das Formular aus der gespeicherten Konfiguration (If-Match mit gelesenem ETag).
  await page.getByRole('button', { name: `Aktionen für ${name}` }).click();
  await page.getByRole('menuitem', { name: 'Bearbeiten' }).click();
  await expect(dlg.getByRole('combobox', { name: 'Aktion Kreditkarte' })).toContainText('reject');
  await pick(page, dlg, 'Fehlermodus', 'open – überspringen');
  await page.getByTestId('binding-form-submit').click();
  await expect(dlg).toBeHidden();
  expect((await api(request, 'GET', `plugin-bindings/${name}`)).body.spec.failMode).toBe('open');
  await api(request, 'DELETE', `plugin-bindings/${name}`, undefined, TOKEN, { 'If-Match': '*' });
  clean(page, { allow: [/status of 422/] });
});

test('Guardrails: Kettenvorschau mit Zwiebelprinzip und override/disable', async ({ page, request }) => {
  const g = `e52-chain-g-${RUN}`;
  const t = `e52-chain-t-${RUN}`;
  const off = `e52-chain-off-${RUN}`;
  await ensure(request, 'plugin-bindings', { metadata: { name: g }, spec: { scope: { level: 'global' }, plugin: 'header', order: 50, config: { request: { set: { 'x-e52': 'g' } } } } });
  await ensure(request, 'plugin-bindings', { metadata: { name: t }, spec: { scope: { level: 'team', target: TEAM }, plugin: 'request-transform', order: 40, config: {} } });
  await useToken(page);
  await gotoReal(page, '/guardrails');
  await page.getByTestId('tab-chain').click();
  const view = page.getByTestId('chain-view');
  await view.getByRole('combobox', { name: 'Team' }).fill(TEAM);
  const steps = view.getByTestId('chain-step');
  await expect(view.getByTestId('chain-steps')).toContainText(t);
  await expect(view.getByTestId('chain-steps')).toContainText(g);
  // Reihenfolge: order 40 (Team) vor order 50 (global).
  const names = await steps.allInnerTexts();
  expect(names.findIndex((s) => s.includes(t))).toBeLessThan(names.findIndex((s) => s.includes(g)));
  await expect(view).toContainText('Antwort/Stream');

  // Team schaltet das globale header-Plugin ab (inherit: disable).
  await ensure(request, 'plugin-bindings', { metadata: { name: off }, spec: { scope: { level: 'team', target: TEAM }, plugin: 'header', inherit: 'disable', config: {} } });
  await gotoReal(page, '/guardrails');
  await page.getByTestId('tab-chain').click();
  await view.getByRole('combobox', { name: 'Team' }).fill(TEAM);
  await expect(view.getByTestId('chain-removed')).toContainText(g);
  await expect(view.getByTestId('chain-steps')).not.toContainText(g);
  await page.screenshot({ path: `${OUT}/screens/echt-plugin-kette-hell-desktop.png` });
  for (const n of [g, t, off]) await api(request, 'DELETE', `plugin-bindings/${n}`, undefined, TOKEN, { 'If-Match': '*' });
  clean(page);
});

test('MCP: Server registrieren (Secret als Verweis), Feldfehler, Freigabe mit Tool-Mustern, ETag-Konflikt', async ({ page, request }) => {
  const secret = `e52-mcp-token-${RUN}`;
  const server = `e52-mcp-${RUN}`;
  const grant = `e52-grant-${RUN}`;
  expect((await api(request, 'POST', 'secrets', { metadata: { name: secret }, spec: { data: { apiKey: { value: 'geheim-e52' } } } })).status).toBe(201);
  await useToken(page);
  await gotoReal(page, '/mcp');
  await page.getByTestId('create-mcp-server').click();
  const dlg = page.getByTestId('mcp-server-form');
  await dlg.getByRole('textbox', { name: 'Name' }).fill(server);
  // SSRF-Regel des Servers → Fehler am URL-Feld.
  await dlg.getByRole('textbox', { name: 'URL' }).fill('http://169.254.169.254/mcp');
  await pick(page, dlg, 'Aktiv', 'aus');
  await pick(page, dlg, 'Anmeldung', 'Secret (empfohlen)');
  await pick(page, dlg, 'Secret', secret);
  await page.getByTestId('mcp-headers').fill('x-tenant: acme');
  await page.getByTestId('mcp-server-form-submit').click();
  await expect(dlg.getByRole('textbox', { name: 'URL' })).toHaveAttribute('aria-invalid', 'true');
  await dlg.getByRole('textbox', { name: 'URL' }).fill('https://mcp.example.org/mcp');
  await page.getByTestId('mcp-server-form-submit').click();
  await expect(dlg).toBeHidden();
  const s = await api(request, 'GET', `mcp-servers/${server}`);
  expect(s.body.spec).toMatchObject({ url: 'https://mcp.example.org/mcp', enabled: false, secret, settings: { headers: { 'x-tenant': 'acme' } } });
  // Kein Klartext des Secrets auf der Seite.
  await expect(page.getByText('geheim-e52')).toHaveCount(0);
  await expect(page.getByTestId('mcp-table')).toContainText('deaktiviert');
  await expect(page.getByTestId('mcp-table')).toContainText(`Secret ${secret}`);

  // Freigabe für das Team: nur search_*, delete_* ausgeschlossen.
  await page.getByTestId('tab-grants').click();
  await page.getByTestId('create-mcp-grant').click();
  const gd = page.getByTestId('mcp-grant-form');
  await gd.getByRole('textbox', { name: 'Name' }).fill(grant);
  await pick(page, gd, 'Server', server);
  await gd.getByRole('combobox', { name: 'Team' }).fill(TEAM);
  await gd.getByRole('textbox', { name: 'Tools' }).fill('search_*\n!delete_*');
  await page.getByTestId('mcp-grant-form-submit').click();
  await expect(gd).toBeHidden();
  const gr = await api(request, 'GET', `mcp-grants/${grant}`);
  expect(gr.body.spec).toMatchObject({ server, subject: { level: 'team', target: TEAM }, tools: ['search_*', '!delete_*'] });
  await expect(page.getByTestId('mcp-grants-table')).toContainText('!delete_*');
  await page.screenshot({ path: `${OUT}/screens/echt-mcp-freigaben-hell-desktop.png` });

  // Optimistische Sperre: zwischen Laden und Speichern ändert jemand die Freigabe → Konflikt-Dialog.
  await page.getByRole('button', { name: `Aktionen für ${grant}` }).click();
  await page.getByRole('menuitem', { name: 'Bearbeiten' }).click();
  await expect(gd.getByRole('textbox', { name: 'Tools' })).toHaveValue('search_*\n!delete_*');
  const upd = await api(request, 'PUT', `mcp-grants/${grant}`, { metadata: { name: grant }, spec: { server, subject: { level: 'team', target: TEAM }, tools: ['*'] } }, TOKEN, { 'If-Match': gr.etag });
  expect(upd.status).toBe(200);
  await gd.getByRole('textbox', { name: 'Tools' }).fill('search_*');
  await page.getByTestId('mcp-grant-form-submit').click();
  await expect(page.getByTestId('conflict-dialog')).toBeVisible();

  await api(request, 'DELETE', `mcp-grants/${grant}`, undefined, TOKEN, { 'If-Match': '*' });
  await api(request, 'DELETE', `mcp-servers/${server}`, undefined, TOKEN, { 'If-Match': '*' });
  await api(request, 'DELETE', `secrets/${secret}`, undefined, TOKEN, { 'If-Match': '*' });
  clean(page, { allow: [/status of 412/, /status of 422/] });
});
