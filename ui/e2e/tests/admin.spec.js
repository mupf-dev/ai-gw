// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// AIGW-49: Verwaltungsseiten gegen das echte `aigw serve` (Bootstrap-Token-Pfad, ohne OIDC):
// Anmeldung per Token-Dialog, Key anlegen mit einmaliger Klartext-Anzeige, Modellgruppe + Mock-Deployment,
// ETag-Konflikt-Dialog, Ausblenden von Schreibaktionen für einen Viewer-Admin-API-Key. Jeweils ohne CSP-Verstöße.
import { api, clean, ensure, expect, gotoReal, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');

test('Anmeldung per Token-Dialog: Mock aus (Standard), echte Daten, Rollen in den Einstellungen', async ({ page }) => {
  // Ohne OIDC und ohne Token öffnet sich der Token-Dialog beim Start von selbst (Mock-Modus ist standardmäßig aus).
  await page.goto('/admin/keys?theme=light');
  const dlg = page.getByTestId('token-dialog');
  await expect(dlg).toBeVisible();
  await expect(page.getByTestId('mock-banner')).toHaveCount(0);
  await dlg.getByTestId('token-input').fill('falsches-token-0123456789-0123456789');
  await dlg.getByTestId('token-submit').click();
  await expect(dlg.getByTestId('form-error')).toContainText('Token ungültig');
  await dlg.getByTestId('token-input').fill(TOKEN);
  await dlg.getByTestId('token-submit').click();
  await expect(dlg).toBeHidden();
  await expect(page.getByTestId('mock-badge')).toHaveCount(0);
  await expect(page.getByTestId('keys-card')).toBeVisible();
  // Token liegt nur im sessionStorage dieses Tabs.
  expect(await page.evaluate(() => sessionStorage.getItem('aigw.adminToken'))).toBe(TOKEN);
  expect(await page.evaluate(() => Object.values(localStorage).some((v) => v.includes('aigw-')))).toBe(false);
  await page.goto('/admin/settings');
  await expect(page.getByTestId('session-roles')).toContainText('Admin (global)');
  await expect(page.getByTestId('session-card')).toContainText('Bootstrap-Token');
  clean(page);
});

test('Key anlegen: Klartext genau einmal, Liste zeigt das Präfix', async ({ page, request }) => {
  // Keys brauchen eine Organisation (F3); ohne Team wählt das Formular die erste.
  await ensure(request, 'organizations', { metadata: { name: 'e2e-org' }, spec: { displayName: 'E2E Org' } });
  await useToken(page);
  await gotoReal(page, '/keys');
  await page.getByTestId('create-key').click();
  await page.getByRole('textbox', { name: 'Name', exact: true }).fill('e2e-key');
  await page.getByTestId('key-form-submit').click();
  const value = page.getByTestId('secret-value');
  await expect(value).toContainText('sk-aigw-');
  const secret = (await value.textContent()).trim();
  expect(secret).toMatch(/^sk-aigw-[A-Za-z0-9]{12}-[A-Za-z0-9]+$/);
  const prefix = secret.slice('sk-aigw-'.length, 'sk-aigw-'.length + 12);
  await expect(page.getByTestId('key-done')).toBeDisabled();
  await page.getByTestId('key-created-dialog').getByRole('checkbox').click();
  await page.getByTestId('key-done').click();
  await expect(page.getByTestId('key-created-dialog')).toBeHidden();
  const row = page.getByTestId('keys-table').locator('tr', { hasText: 'e2e-key' });
  await expect(row.getByTestId('key-prefix')).toHaveText(`sk-aigw-${prefix}…`);
  // Nach dem Schließen ist der Klartext nirgends mehr im DOM – auch nicht nach dem Neuladen.
  expect(await page.content()).not.toContain(secret);
  await page.reload();
  await expect(page.getByTestId('keys-table')).toContainText('e2e-key');
  expect(await page.content()).not.toContain(secret);
  // Die API liefert ebenfalls nur das Präfix.
  const k = await api(request, 'GET', 'api-keys/e2e-key');
  expect(k.body.status.prefix).toBe(prefix);
  expect(JSON.stringify(k.body)).not.toContain(secret);
  clean(page);
});

test('Modellgruppe und Mock-Deployment anlegen', async ({ page, request }) => {
  await useToken(page);
  await gotoReal(page, '/models');
  await page.getByTestId('create-group').click();
  const g = page.getByTestId('group-form-dialog');
  await g.getByRole('textbox', { name: 'Name', exact: true }).fill('e2e-group');
  await g.getByRole('textbox', { name: /Aliasse/ }).fill('e2e-alias, e2e/alias');
  await page.getByTestId('group-form-submit').click();
  await expect(g).toBeHidden();
  const detail = page.getByTestId('group-detail');
  await expect(detail).toContainText('e2e-group');
  await expect(detail).toContainText('e2e/alias');

  await page.getByTestId('create-deployment').click();
  const d = page.getByTestId('deployment-form-dialog');
  await d.getByRole('textbox', { name: 'Name', exact: true }).fill('e2e-echo');
  await d.getByRole('combobox', { name: /Anbieter/ }).click();
  await page.getByRole('option', { name: 'Mock (Test)' }).click();
  await d.getByRole('textbox', { name: /Modell beim Anbieter/ }).fill('mock');
  await page.getByTestId('deployment-form-submit').click();
  await expect(d).toBeHidden();
  await expect(page.getByTestId('deployments-table')).toContainText('e2e-echo');

  const dep = await api(request, 'GET', 'deployments/e2e-echo');
  expect(dep.status).toBe(200);
  expect(dep.body.spec).toMatchObject({ modelGroup: 'e2e-group', provider: 'mock', providerModel: 'mock', enabled: true });
  clean(page);
});

test('ETag-Konflikt: paralleles Ändern → Dialog „neu laden“', async ({ page, request }) => {
  await ensure(request, 'organizations', { metadata: { name: 'e2e-org' }, spec: { displayName: 'E2E Org' } });
  await ensure(request, 'teams', { metadata: { name: 'e2e-team' }, spec: { organization: 'e2e-org', displayName: 'Vorher' } });
  await useToken(page);
  await gotoReal(page, '/teams?tab=team');
  await page.getByRole('button', { name: 'Aktionen für e2e-team' }).click();
  await page.getByRole('menuitem', { name: 'Bearbeiten' }).click();
  const form = page.getByTestId('entity-form');
  const display = form.getByRole('textbox', { name: /Anzeigename/ });
  await expect(display).toHaveValue('Vorher');

  // Jemand anderes ändert das Team, während der Dialog offen ist.
  const cur = await api(request, 'GET', 'teams/e2e-team');
  const put = await api(request, 'PUT', 'teams/e2e-team', { metadata: { name: 'e2e-team' }, spec: { ...cur.body.spec, displayName: 'Von nebenan' } }, undefined, {
    'if-match': cur.etag,
  });
  expect(put.status).toBe(200);

  await display.fill('Meine Änderung');
  await page.getByTestId('entity-form-submit').click();
  const conflict = page.getByTestId('conflict-dialog');
  await expect(conflict).toBeVisible();
  await expect(conflict).toContainText('Zwischenzeitlich geändert');
  await conflict.getByTestId('conflict-reload').click();
  await expect(conflict).toBeHidden();
  await expect(display).toHaveValue('Von nebenan');
  // Jetzt mit aktuellem ETag speichern.
  await display.fill('Meine Änderung');
  await page.getByTestId('entity-form-submit').click();
  await expect(form).toBeHidden();
  expect((await api(request, 'GET', 'teams/e2e-team')).body.spec.displayName).toBe('Meine Änderung');
  clean(page, { allow: [/status of 412/] });
});

test('Rollen: Viewer-Admin-API-Key sieht keine Schreibaktionen', async ({ page, request }) => {
  // If-Match ist Pflicht (F38); `*` löscht ohne Versionsvergleich.
  await api(request, 'DELETE', 'admin-api-keys/e2e-viewer', undefined, undefined, { 'if-match': '*' });
  const created = await api(request, 'POST', 'admin-api-keys', { name: 'e2e-viewer', description: 'E2E nur lesen', roles: [{ role: 'viewer' }] });
  expect(created.status).toBe(201);
  // Daten, die der Viewer sehen soll
  await ensure(request, 'organizations', { metadata: { name: 'e2e-org' }, spec: {} });
  await ensure(request, 'teams', { metadata: { name: 'e2e-team' }, spec: { organization: 'e2e-org' } });
  await ensure(request, 'api-keys', { metadata: { name: 'e2e-viewer-sees' }, spec: { organization: 'e2e-org', team: 'e2e-team' } });

  await useToken(page, created.body.secret);
  await gotoReal(page, '/keys');
  await expect(page.getByTestId('keys-table')).toContainText('e2e-viewer-sees');
  await expect(page.getByTestId('create-key')).toHaveCount(0);
  await page.getByRole('button', { name: 'Aktionen für e2e-viewer-sees' }).click();
  await expect(page.getByRole('menuitem', { name: /Name kopieren/ })).toBeVisible();
  await expect(page.getByRole('menuitem', { name: /Bearbeiten|Sperren|Rotieren|Löschen/ })).toHaveCount(0);
  await page.keyboard.press('Escape');

  await gotoReal(page, '/models');
  await expect(page.getByTestId('create-group')).toHaveCount(0);
  await expect(page.getByTestId('edit-group')).toHaveCount(0);
  await expect(page.getByTestId('sidebar').getByRole('link', { name: 'Admin-API-Keys' })).toHaveCount(0);

  await gotoReal(page, '/teams?tab=team');
  await expect(page.getByTestId('create-team')).toHaveCount(0);
  await gotoReal(page, '/providers');
  await expect(page.getByTestId('create-credential')).toHaveCount(0);
  await gotoReal(page, '/settings');
  await expect(page.getByTestId('session-roles')).toContainText('Betrachter (global)');
  // YAML-Import/-Export (AIGW-55): Betrachter dürfen exportieren, aber nicht anwenden.
  await gotoReal(page, '/config');
  await expect(page.getByTestId('gitops-export')).toBeVisible();
  await expect(page.getByTestId('gitops-apply')).toHaveCount(0);
  await expect(page.getByTestId('gitops-import-card')).toContainText('vorbehalten');
  clean(page);
});
