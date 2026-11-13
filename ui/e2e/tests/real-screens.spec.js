// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// AIGW-49: Screenshots der echten Verwaltungsseiten (Daten per Admin-API angelegt, Anmeldung per Bootstrap-Token),
// hell/dunkel × Desktop, für docs/konzept/ux/ (Präfix „echt-“). Ebenfalls ohne CSP-Verstöße.
import { api, clean, ensure, expect, gotoReal, OUT, test, TOKEN, useToken } from './fixtures.js';

test.skip(!TOKEN, 'AIGW_TOKEN fehlt (run.sh setzt das Bootstrap-Token)');
test.use({ reducedMotion: 'reduce' });

test.beforeAll(async ({ request }) => {
  const orgs = [
    ['acme', 'Acme GmbH'],
    ['labs', 'Acme Labs'],
  ];
  for (const [name, displayName] of orgs) await ensure(request, 'organizations', { metadata: { name }, spec: { displayName } });
  const teams = [
    ['platform', 'acme', 'Plattform'],
    ['data-science', 'acme', 'Data Science'],
    ['support', 'acme', 'Support-Assistent'],
    ['research', 'labs', 'Research'],
  ];
  for (const [name, organization, displayName] of teams) await ensure(request, 'teams', { metadata: { name }, spec: { organization, displayName } });
  await ensure(request, 'users', { metadata: { name: 'anna' }, spec: { displayName: 'Anna Becker', email: 'a.becker@acme.example', roles: [{ role: 'admin' }] } });
  await ensure(request, 'users', {
    metadata: { name: 'ben' },
    spec: { displayName: 'Ben Kaya', email: 'b.kaya@acme.example', roles: [{ role: 'team-admin', organization: 'acme', team: 'platform' }] },
  });
  await ensure(request, 'users', { metadata: { name: 'carla' }, spec: { displayName: 'Carla Nowak', email: 'c.nowak@acme.example', roles: [{ role: 'org-admin', organization: 'acme' }] } });

  const fake = (s) => ({ value: `${s}-nur-fuer-tests-0123456789` });
  await ensure(request, 'provider-credentials', { metadata: { name: 'openai-prod', labels: { env: 'prod' } }, spec: { provider: 'openai', data: { apiKey: fake('sk') } } });
  await ensure(request, 'provider-credentials', { metadata: { name: 'azure-swc', labels: { env: 'prod' } }, spec: { provider: 'azure-openai', data: { apiKey: fake('az') } } });
  await ensure(request, 'provider-credentials', { metadata: { name: 'anthropic-prod' }, spec: { provider: 'anthropic', data: { apiKey: fake('ant') } } });

  await ensure(request, 'model-groups', { metadata: { name: 'claude-sonnet' }, spec: { aliases: ['anthropic/claude-sonnet'], routing: 'least-latency' } });
  await ensure(request, 'model-groups', { metadata: { name: 'demo-mock' }, spec: { routing: 'weighted', settings: { retry: { maxAttempts: 2, initialBackoffMs: 50 } } } });
  await ensure(request, 'model-groups', {
    metadata: { name: 'gpt-4o' },
    spec: {
      aliases: ['gpt-4o-latest', 'openai/gpt-4o'],
      routing: 'weighted',
      fallbacks: [{ modelGroup: 'claude-sonnet', on: ['rate-limit', 'unavailable'] }],
      settings: { retry: { maxAttempts: 3, initialBackoffMs: 100 }, circuitBreaker: { failureRate: 0.5, minRequests: 20 } },
    },
  });
  const deps = [
    ['gpt-4o-azure-swc', 'gpt-4o', 'azure-openai', 'gpt4o-ptu', 'azure-swc', 0, 3, { rpm: 3000, tpm: 450000 }, 'https://acme-swc.openai.azure.com', { apiVersion: '2024-10-21' }],
    ['gpt-4o-openai', 'gpt-4o', 'openai', 'gpt-4o-2024-11-20', 'openai-prod', 1, 1, { rpm: 10000 }, null, {}],
    ['claude-sonnet-anthropic', 'claude-sonnet', 'anthropic', 'claude-sonnet-4-5', 'anthropic-prod', 0, 1, { rpm: 4000 }, null, {}],
    ['demo-mock-echo', 'demo-mock', 'mock', 'mock', null, 0, 1, {}, null, {}],
  ];
  for (const [name, modelGroup, provider, providerModel, credential, priority, weight, capacity, endpoint, settings] of deps) {
    await ensure(request, 'deployments', { metadata: { name }, spec: { modelGroup, provider, providerModel, credential, priority, weight, capacity, endpoint, settings, enabled: true } });
  }
  const prices = [
    ['openai', 'gpt-4o-2024-11-20', '2025-01-01T00:00:00Z', { input: '2.75', cachedInput: '1.375', output: '11' }],
    ['openai', 'gpt-4o-2024-11-20', '2026-06-01T00:00:00Z', { input: '2.50', cachedInput: '1.25', output: '10' }],
    ['openai', 'gpt-4o-2024-11-20', '2027-01-01T00:00:00Z', { input: '2.20', cachedInput: '1.10', output: '9' }],
    ['anthropic', 'claude-sonnet-4-5', '2025-09-29T00:00:00Z', { input: '3', cachedInput: '0.30', cacheWrite: '3.75', output: '15' }],
    [null, 'gpt4o-ptu', '2026-01-01T00:00:00Z', { input: '0', output: '0' }],
  ];
  for (const [provider, model, validFrom, perMillionTokens] of prices) await ensure(request, 'prices', { spec: { provider, model, validFrom, perMillionTokens } });

  const keys = [
    ['support-bot-prod', 'support', ['gpt-4o'], { amount: '250', period: 'month', hardLimit: true }, [{ metric: 'requests', window: 'minute', limit: 600 }], '2027-03-31T00:00:00Z'],
    ['platform-ci', 'platform', [], null, [{ metric: 'tokens', window: 'minute', limit: 200000 }], null],
    ['notebook-anna', 'data-science', ['gpt-4o', 'claude-sonnet'], { amount: '50', period: 'week', hardLimit: true }, [], new Date(Date.now() + 7 * 86400000).toISOString()],
    ['research-batch', 'research', ['demo-mock'], null, [], null],
  ];
  // F3: Organisation Pflicht – die des Teams.
  const orgOf = Object.fromEntries(teams.map(([t, o]) => [t, o]));
  for (const [name, team, allowedModels, budget, limits, expiresAt] of keys)
    await ensure(request, 'api-keys', { metadata: { name }, spec: { organization: orgOf[team], team, allowedModels, budget, limits, expiresAt } });
  const old = await api(request, 'GET', 'api-keys/research-batch');
  if (old.body?.status?.state === 'active') await api(request, 'POST', 'api-keys/research-batch/revoke', {}, undefined, { 'if-match': old.etag });
  const ak = await api(request, 'GET', 'admin-api-keys/grafana-readonly');
  if (ak.status === 404) await ensure(request, 'admin-api-keys', { name: 'grafana-readonly', description: 'Dashboards, nur lesen', roles: [{ role: 'viewer' }], expiresAt: '2027-06-30T00:00:00Z' });
});

const SCENES = [
  { name: 'keys', path: '/keys', ready: 'keys-table' },
  { name: 'modelle', path: '/models?group=gpt-4o', ready: 'deployments-table' },
  { name: 'anbieter', path: '/providers', ready: 'credentials-table' },
  { name: 'preise', path: '/prices', ready: 'prices-table' },
  { name: 'teams', path: '/teams?tab=team', ready: 'teams-table' },
  { name: 'nutzer', path: '/teams?tab=user', ready: 'users-table' },
  { name: 'admin-keys', path: '/admin-keys', ready: 'admin-keys-table' },
  { name: 'einstellungen', path: '/settings', ready: 'session-card' },
  {
    name: 'deployment-bearbeiten',
    path: '/models?group=gpt-4o',
    ready: 'deployments-table',
    act: async (page) => {
      await page.getByRole('button', { name: 'Aktionen für gpt-4o-azure-swc' }).click();
      await page.getByRole('menuitem', { name: 'Bearbeiten' }).click();
      await expect(page.getByTestId('deployment-form-dialog').getByRole('textbox', { name: /Modell beim Anbieter/ })).toHaveValue('gpt4o-ptu');
    },
  },
  {
    name: 'zugangsdaten-anlegen',
    path: '/providers',
    ready: 'credentials-table',
    act: async (page) => {
      await page.getByTestId('create-credential').click();
      const d = page.getByTestId('credential-form-dialog');
      await d.getByRole('combobox').first().click();
      await page.getByRole('option', { name: 'Azure OpenAI' }).click();
      await d.getByText('Entra ID (Service Principal)').click();
    },
  },
  {
    name: 'gitops-trockenlauf',
    path: '/config',
    ready: 'gitops-import-card',
    act: async (page) => {
      const yaml = [
        'apiVersion: aigw/v1alpha1',
        'kind: ModelGroup',
        'metadata: {name: gpt-4o}',
        'spec:',
        '  aliases: [gpt-4o-latest, openai/gpt-4o, gpt-4o-eu]',
        '  routing: least-latency',
        '  fallbacks: [{modelGroup: claude-sonnet, on: [rate-limit, unavailable]}]',
        '  settings: {retry: {maxAttempts: 3, initialBackoffMs: 100}, circuitBreaker: {failureRate: 0.5, minRequests: 20}}',
        '---',
        'apiVersion: aigw/v1alpha1',
        'kind: Team',
        'metadata: {name: legal}',
        'spec: {organization: acme, displayName: Recht & Compliance}',
      ].join('\n');
      await page.getByTestId('gitops-yaml').fill(yaml);
      await page.getByTestId('gitops-plan').click();
      await expect(page.getByTestId('apply-plan')).toBeVisible();
      await page.getByTestId('apply-plan').scrollIntoViewIfNeeded();
    },
  },
];

for (const theme of ['hell', 'dunkel']) {
  for (const s of SCENES) {
    if (s.act && theme === 'dunkel' && s.name !== 'gitops-trockenlauf') continue;
    test(`echt ${s.name} ${theme}`, async ({ page }) => {
      await useToken(page);
      await gotoReal(page, s.path, { theme: theme === 'hell' ? 'light' : 'dark' });
      await expect(page.getByTestId(s.ready)).toBeVisible();
      await page.waitForTimeout(300);
      if (s.act) await s.act(page);
      await page.waitForTimeout(400);
      await page.screenshot({ path: `${OUT}/screens/echt-${s.name}-${theme}-desktop.png`, fullPage: !s.act });
      clean(page);
    });
  }
}

test('echt keys schmal', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await useToken(page);
  await gotoReal(page, '/keys');
  await expect(page.getByTestId('keys-table')).toBeVisible();
  await page.waitForTimeout(300);
  await page.screenshot({ path: `${OUT}/screens/echt-keys-hell-schmal.png`, fullPage: true });
  clean(page);
});
