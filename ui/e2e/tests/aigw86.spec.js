// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-86 gegen das echte `aigw serve`: Standard-Guardrail-Profile – Karten mit Erklärung und Vorschau, „Anwenden
// auf …“ mit Scope, Diff (Dry-Run), Anwenden, Idempotenz (zweiter Diff: unverändert), Profilwechsel. Ohne CSP-Verstöße.
import { api, clean, ensure, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

const ORG = 'e86-org';
const TEAM = 'e86-team';

test.beforeAll(async ({ request }) => {
  await ensure(request, 'organizations', { metadata: { name: ORG }, spec: { displayName: 'AIGW-86 Org' } });
  await ensure(request, 'teams', { metadata: { name: TEAM }, spec: { organization: ORG, displayName: 'AIGW-86 Team' } });
});

async function applyTo(page, profile, team) {
  await page.getByTestId(`apply-${profile}`).click();
  const dlg = page.getByTestId('profile-apply');
  await dlg.getByRole('combobox', { name: 'Ziel' }).fill(team);
  await page.getByTestId('profile-apply-submit').click();
  await expect(page.getByTestId('profile-diff')).toBeVisible();
  return dlg;
}

test('Guardrail-Profile: Vorschau, Diff, Anwenden, Idempotenz, Wechsel', async ({ page, request }) => {
  await useToken(page);
  await gotoReal(page, '/guardrails?tab=profiles');
  const cards = page.getByTestId('guardrail-profiles');
  await expect(cards).toContainText('Basis');
  await expect(cards).toContainText('Streng');
  await expect(cards).toContainText('Offen');
  await page.getByTestId('profile-preview-basis').locator('summary').click();
  await expect(page.getByTestId('profile-preview-basis')).toContainText('"plugin": "pii"');
  await page.screenshot({ path: `${OUT}/screens/echt-guardrail-profile-hell-desktop.png`, fullPage: true });

  // Basis: Diff mit zwei neuen Bindungen, dann anwenden.
  let dlg = await applyTo(page, 'basis', TEAM);
  const diff = page.getByTestId('profile-diff');
  await expect(diff).toContainText(`basis-pii-team-${TEAM}`);
  await expect(diff).toContainText(`basis-prompt-filter-team-${TEAM}`);
  await expect(diff).toContainText('neu');
  await page.screenshot({ path: `${OUT}/screens/echt-guardrail-profil-diff-hell-desktop.png` });
  await page.getByTestId('profile-apply-submit').click();
  await expect(dlg).toBeHidden();
  const b = await api(request, 'GET', `plugin-bindings/basis-pii-team-${TEAM}`);
  expect(b.status).toBe(200);
  expect(b.body.metadata.labels['aigw.io/guardrail-profile']).toBe('basis');

  // Nochmal: alles unverändert.
  dlg = await applyTo(page, 'basis', TEAM);
  await expect(page.getByTestId('profile-diff')).toContainText('unverändert');
  await expect(page.getByTestId('profile-diff')).not.toContainText('neu');
  await page.keyboard.press('Escape');
  await expect(dlg).toBeHidden();

  // Wechsel auf Offen: Basis-Bindungen werden entfernt, Logging-Obergrenze am Team.
  dlg = await applyTo(page, 'offen', TEAM);
  await expect(page.getByTestId('profile-diff')).toContainText('entfernen');
  await page.getByTestId('profile-apply-submit').click();
  await expect(dlg).toBeHidden();
  expect((await api(request, 'GET', `plugin-bindings/basis-pii-team-${TEAM}`)).status).toBe(404);
  expect((await api(request, 'GET', `teams/${TEAM}`)).body.spec.payloadModeMax).toBe('metadata');
  await api(request, 'DELETE', `plugin-bindings/offen-prompt-filter-team-${TEAM}`, undefined, TOKEN, { 'If-Match': '*' });
  clean(page);
});
