// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Mock-Backend der Admin-API (AIGW-49): beantwortet im Mock-Modus dieselben Anfragen wie der Server –
// gleiche Pfade, Körper, ETag/If-Match (412), Cursor-Pagination, Problem Details (RFC 9457) und die
// wichtigsten Prüfungen (Namen, Verweise, Löschsperren, Pflichtfelder der Zugangsdaten). So laufen die
// Verwaltungsseiten im Mock- und im Echtbetrieb über denselben Code. Zustand nur im Speicher (Tab).
// Wird erst im Mock-Modus nachgeladen (eigener Chunk).
import type { components } from '../api/schema';
import { CREDENTIAL_FIELDS, NAME_RE } from '../api/resources';
import { MOCK, MODEL_GROUPS, ORGS } from './data';

type S = components['schemas'];
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type Obj = Record<string, any>;
interface Entry {
  obj: Obj;
  v: number;
}

const KINDS = ['organizations', 'teams', 'users', 'api-keys', 'model-groups', 'deployments', 'provider-credentials', 'prices', 'admin-api-keys'] as const;
type Kind = (typeof KINDS)[number];

class Problem extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    readonly title: string,
    readonly detail?: string,
  ) {
    super(title);
  }
}
const notFound = (kind: string, name: string) => new Problem(404, 'not-found', 'Nicht gefunden', `${kind} „${name}“ existiert nicht`);
const invalid = (detail: string) => new Problem(422, 'validation-failed', 'Ungültige Eingabe', detail);
const conflictP = (detail: string) => new Problem(409, 'conflict', 'Konflikt', detail);

let seq = 0;
const now = () => new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
const uuid = () => `0190${(++seq).toString(16).padStart(4, '0')}-0000-7000-8000-${Math.random().toString(16).slice(2, 14).padEnd(12, '0')}`;
const B62 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
function rand(n: number): string {
  const a = new Uint8Array(n);
  crypto.getRandomValues(a);
  return [...a].map((x) => B62[x % 62]).join('');
}

let db: Record<Kind, Map<string, Entry>> | null = null;

function put(kind: Kind, obj: Obj) {
  const name = kind === 'admin-api-keys' ? obj.name : obj.metadata.name;
  store()[kind].set(name, { obj, v: (store()[kind].get(name)?.v ?? 0) + 1 });
}
const all = (kind: Kind) => [...store()[kind].values()].map((e) => e.obj);
const has = (kind: Kind, name: string | null | undefined) => !!name && store()[kind].has(name);
function entry(kind: Kind, name: string): Entry {
  const e = store()[kind].get(name);
  if (!e) throw notFound(kind, name);
  return e;
}
const etag = (kind: Kind, name: string, e: Entry) => `"mock-${kind}-${name}-${e.v}"`;

function meta(name: string, labels: Record<string, string> = {}, prev?: Obj) {
  const t = now();
  return { name, labels, createdAt: prev?.metadata?.createdAt ?? t, updatedAt: t };
}

// ------------------------------------------------------------------ Beispieldaten

function seed(): Record<Kind, Map<string, Entry>> {
  db = Object.fromEntries(KINDS.map((k) => [k, new Map()])) as Record<Kind, Map<string, Entry>>;
  const t0 = '2026-09-01T08:00:00Z';
  const m = (name: string, labels: Record<string, string> = {}) => ({ name, labels, createdAt: t0, updatedAt: t0 });
  for (const o of ORGS) {
    put('organizations', { id: uuid(), metadata: m(o.id, { plan: o.plan.toLowerCase() }), spec: { displayName: o.name, limits: [] } });
    for (const t of o.teams) put('teams', { id: uuid(), metadata: m(t.id), spec: { organization: o.id, displayName: t.name, allowedModels: [], limits: [] } });
  }
  const users: [string, string, string, S['RoleBinding'][]][] = [
    ['anna', 'Anna Becker', 'a.becker@acme.example', [{ role: 'admin' }]],
    ['ben', 'Ben Kaya', 'b.kaya@acme.example', [{ role: 'team-admin', organization: 'acme', team: 'platform' }]],
    ['carla', 'Carla Nowak', 'c.nowak@acme.example', [{ role: 'org-admin', organization: 'acme' }]],
    ['dieter', 'Dieter Hahn', 'd.hahn@acme.example', [{ role: 'viewer' }]],
  ];
  for (const [name, displayName, email, roles] of users) {
    put('users', { id: uuid(), metadata: m(name), spec: { displayName, email, oidcIssuer: 'https://idp.acme.example/realms/acme', oidcSubject: `sub-${name}`, roles } });
  }
  const PROVIDER: Record<string, S['ProviderKind']> = { azure: 'azure-openai' };
  const prov = (p: string): S['ProviderKind'] => PROVIDER[p] ?? (p as S['ProviderKind']);
  const credFields: Partial<Record<S['ProviderKind'], string[]>> = {
    'azure-openai': ['apiKey'],
    bedrock: ['accessKeyId', 'secretAccessKey'],
    vertex: ['serviceAccountJson'],
    vllm: ['apiKey'],
  };
  for (const p of new Set(MODEL_GROUPS.flatMap((g) => g.deployments.map((d) => prov(d.provider))))) {
    const fields = credFields[p] ?? ['apiKey'];
    put('provider-credentials', { id: uuid(), metadata: m(`${p}-prod`, { env: 'prod' }), spec: { provider: p, fields }, status: { hasSecret: true, keyId: 'a3f1c0de' } });
  }
  const ROUTING: Record<string, S['RoutingStrategy']> = { priority: 'weighted', weighted: 'weighted', latency: 'least-latency', cost: 'cost' };
  for (const g of MODEL_GROUPS) {
    put('model-groups', {
      id: uuid(),
      metadata: m(g.alias, { modality: g.modality }),
      spec: {
        aliases: g.alias === 'gpt-4o' ? ['gpt-4o-latest', 'openai/gpt-4o'] : g.alias === 'claude-sonnet' ? ['anthropic/claude-sonnet'] : [],
        routing: ROUTING[g.strategy],
        fallbacks: g.fallbacks.map((f) => ({ modelGroup: f, on: ['rate-limit', 'unavailable'] })),
        limits: [],
        settings: { retry: { maxAttempts: 2 } },
      },
    });
    for (const d of g.deployments) {
      const p = prov(d.provider);
      const settings: Obj = p === 'azure-openai' ? { apiVersion: '2024-10-21' } : p === 'bedrock' ? { region: d.region } : {};
      put('deployments', {
        id: uuid(),
        metadata: m(d.id, { region: d.region }),
        spec: {
          modelGroup: g.alias,
          provider: p,
          providerModel: d.upstreamModel,
          endpoint: p === 'azure-openai' ? `https://acme-${d.region}.openai.azure.com` : p === 'vllm' ? 'https://vllm.rz-nord.acme.example/v1' : null,
          credential: `${p}-prod`,
          priority: d.priority - 1,
          weight: d.weight,
          capacity: { rpm: d.rpmLimit, tpm: null },
          enabled: d.health !== 'down',
          settings,
        },
      });
    }
    const d0 = g.deployments[0];
    if (d0 && g.priceIn > 0) {
      const add = (validFrom: string, k: number) =>
        put('prices', {
          id: uuid(),
          metadata: m(priceName(prov(d0.provider), d0.upstreamModel, validFrom)),
          spec: { provider: prov(d0.provider), model: d0.upstreamModel, validFrom, perMillionTokens: { input: (g.priceIn * k).toFixed(2), output: (g.priceOut * k).toFixed(2) } },
        });
      add('2025-01-01T00:00:00Z', 1.1);
      add('2026-06-01T00:00:00Z', 1);
      if (g.alias === 'gpt-4o') add('2027-01-01T00:00:00Z', 0.9);
    }
  }
  for (const k of MOCK.keys) {
    const limits: S['LimitSpec'][] = [];
    if (k.rpm) limits.push({ metric: 'requests', window: 'minute', limit: k.rpm });
    if (k.tpm) limits.push({ metric: 'tokens', window: 'minute', limit: k.tpm });
    const expiresAt = k.status === 'expired' ? new Date(Date.now() - 86_400_000).toISOString() : k.expiresAt ? new Date(k.expiresAt).toISOString() : null;
    put('api-keys', {
      id: uuid(),
      metadata: { name: k.name, labels: {}, createdAt: new Date(k.createdAt).toISOString(), updatedAt: new Date(k.createdAt).toISOString() },
      spec: {
        team: k.teamId,
        allowedModels: k.models === 'all' ? [] : k.models,
        budget: k.budgetLimit ? { amount: String(k.budgetLimit), period: 'month', hardLimit: true, warnAtPercent: [80] } : null,
        limits,
        expiresAt,
        metadata: {},
      },
      status: { prefix: `${k.prefix.replace(/[^A-Za-z0-9]/g, '').slice(0, 8)}${k.last4}`.padEnd(12, 'x').slice(0, 12), state: k.status === 'revoked' ? 'revoked' : 'active', lastUsedAt: k.lastUsedAt ? new Date(k.lastUsedAt).toISOString() : null },
    });
  }
  const adminKey = (name: string, description: string, roles: S['RoleBinding'][]): S['AdminApiKey'] => ({
    id: uuid(),
    name,
    description,
    prefix: rand(12),
    roles,
    createdBy: 'user:anna',
    createdAt: t0,
    updatedAt: t0,
    expiresAt: '2027-06-30T00:00:00Z',
    lastUsedAt: null,
  });
  put('admin-api-keys', adminKey('gitops-ci', 'GitOps-Pipeline (config:apply)', [{ role: 'admin' }]));
  put('admin-api-keys', adminKey('grafana-readonly', 'Dashboards, nur lesen', [{ role: 'viewer' }]));
  return db;
}
function store() {
  return db ?? seed();
}

function priceName(provider: string | null | undefined, model: string, validFrom: string): string {
  const d = validFrom.replace(/[-:]/g, '').replace(/T(\d{6}).*$/, (_, t: string) => (t === '000000' ? '' : t)).slice(0, 14);
  return `${provider ?? 'any'}-${model.toLowerCase().replace(/[^a-z0-9._-]/g, '-')}-${d}`;
}

// ------------------------------------------------------------------ Prüfungen

function checkName(name: unknown): string {
  if (typeof name !== 'string' || !NAME_RE.test(name)) throw invalid(`ungültiger Name ${JSON.stringify(name)}: erlaubt sind a-z, 0-9, '-', '.', '_' (max. 128, beginnt alphanumerisch)`);
  return name;
}

function groupOf(nameOrAlias: string): string | null {
  for (const g of all('model-groups')) if (g.metadata.name === nameOrAlias || (g.spec.aliases ?? []).includes(nameOrAlias)) return g.metadata.name;
  return null;
}

function validate(kind: Kind, name: string, spec: Obj) {
  const models = (list: string[] | undefined) => {
    for (const x of list ?? []) if (!groupOf(x)) throw invalid(`unbekannte Modellgruppe „${x}“ in allowedModels`);
  };
  switch (kind) {
    case 'teams':
      if (!has('organizations', spec.organization)) throw invalid(`unbekannte Organisation „${spec.organization}“`);
      models(spec.allowedModels);
      break;
    case 'api-keys':
      if (spec.team && !has('teams', spec.team)) throw invalid(`unbekanntes Team „${spec.team}“`);
      if (spec.user && !has('users', spec.user)) throw invalid(`unbekannter Nutzer „${spec.user}“`);
      models(spec.allowedModels);
      break;
    case 'model-groups': {
      for (const a of spec.aliases ?? []) {
        if (a === name) throw invalid('der eigene Name ist kein Alias');
        const owner = groupOf(a);
        if (owner && owner !== name) throw conflictP(`Alias „${a}“ ist bereits Name oder Alias der Gruppe „${owner}“`);
      }
      for (const f of spec.fallbacks ?? []) {
        if (f.modelGroup === name) throw invalid('Fallback auf die Gruppe selbst');
        if (!has('model-groups', f.modelGroup)) throw invalid(`Fallback-Ziel „${f.modelGroup}“ ist keine Modellgruppe (kanonischen Namen verwenden)`);
      }
      break;
    }
    case 'deployments': {
      if (!has('model-groups', spec.modelGroup)) throw invalid(`unbekannte Modellgruppe „${spec.modelGroup}“`);
      if (spec.credential) {
        const c = store()['provider-credentials'].get(spec.credential);
        if (!c) throw invalid(`unbekannte Zugangsdaten „${spec.credential}“`);
        if (c.obj.spec.provider !== spec.provider) throw invalid(`Zugangsdaten „${spec.credential}“ gehören zu ${c.obj.spec.provider}, nicht zu ${spec.provider}`);
      }
      if (['azure-openai', 'vllm', 'openai-compatible'].includes(spec.provider) && !spec.endpoint) throw invalid(`endpoint ist Pflicht für ${spec.provider}`);
      if (spec.endpoint && !/^https:\/\/[^/@\s]+/.test(spec.endpoint)) throw invalid('endpoint: nur https:// ohne Zugangsdaten (Mock: vereinfachte Prüfung)');
      if (spec.weight !== undefined && (spec.weight < 1 || spec.weight > 1_000_000)) throw invalid('weight muss 1–1 000 000 sein');
      if (!spec.providerModel) throw invalid('providerModel fehlt');
      break;
    }
  }
}

function checkCredentialFields(provider: S['ProviderKind'], fields: string[]) {
  const req = CREDENTIAL_FIELDS[provider];
  if (!req) throw invalid(`unbekannter Anbieter ${provider}`);
  if (req.alternatives.length && !req.alternatives.some((a) => a.fields.every((f) => fields.includes(f)))) {
    throw invalid(`Pflichtfelder für ${provider} fehlen: ${req.alternatives.map((a) => a.fields.join(' + ')).join(' oder ')}`);
  }
}

function blockDelete(kind: Kind, name: string) {
  const refs = (k: Kind, f: (o: Obj) => boolean, what: string) => {
    const n = all(k).filter(f).length;
    if (n) throw conflictP(`„${name}“ wird noch verwendet (${n} ${what})`);
  };
  if (kind === 'organizations') refs('teams', (t) => t.spec.organization === name, 'Team(s)');
  if (kind === 'teams') refs('api-keys', (k) => k.spec.team === name, 'API-Key(s)');
  if (kind === 'users') refs('api-keys', (k) => k.spec.user === name, 'API-Key(s)');
  if (kind === 'model-groups') {
    refs('deployments', (d) => d.spec.modelGroup === name, 'Deployment(s)');
    refs('model-groups', (g) => (g.spec.fallbacks ?? []).some((f: Obj) => f.modelGroup === name), 'Fallback-Verweis(e)');
  }
  if (kind === 'provider-credentials') refs('deployments', (d) => d.spec.credential === name, 'Deployment(s)');
  if (kind === 'prices') {
    const p = entry('prices', name).obj;
    if (new Date(p.spec.validFrom) <= new Date()) throw conflictP('Preisversion ist bereits gültig – neue Version anlegen statt löschen');
  }
}

function checkIfMatch(kind: Kind, name: string, e: Entry, headers: Headers) {
  const want = headers.get('If-Match');
  if (!want || want === '*') return;
  if (!want.split(',').map((s) => s.trim()).includes(etag(kind, name, e))) {
    throw new Problem(412, 'precondition-failed', 'Ressource wurde zwischenzeitlich geändert', 'If-Match passt nicht zum aktuellen ETag – neu laden und erneut versuchen');
  }
}

// ------------------------------------------------------------------ Listen

function matches(kind: Kind, o: Obj, q: URLSearchParams): boolean {
  const needle = q.get('q')?.toLowerCase();
  if (needle) {
    const hay = [o.metadata.name, ...(kind === 'model-groups' ? (o.spec.aliases ?? []) : []), kind === 'api-keys' ? o.status.prefix : ''].join(' ').toLowerCase();
    if (!hay.includes(needle)) return false;
  }
  const eq = (param: string, value: unknown) => !q.has(param) || q.get(param) === String(value ?? '');
  switch (kind) {
    case 'teams':
      return eq('organization', o.spec.organization);
    case 'users':
      return !q.has('email') || (o.spec.email ?? '').toLowerCase() === q.get('email')?.toLowerCase();
    case 'api-keys': {
      const org = o.spec.team ? store().teams.get(o.spec.team)?.obj.spec.organization : null;
      return eq('team', o.spec.team) && eq('user', o.spec.user) && eq('state', o.status.state) && eq('organization', org);
    }
    case 'deployments':
      return eq('modelGroup', o.spec.modelGroup) && eq('provider', o.spec.provider) && eq('credential', o.spec.credential) && eq('enabled', o.spec.enabled !== false);
    case 'provider-credentials':
      return eq('provider', o.spec.provider);
    case 'prices':
      return eq('model', o.spec.model) && (!q.has('provider') || (q.get('provider') === 'any' ? !o.spec.provider : o.spec.provider === q.get('provider')));
  }
  return true;
}

function list(kind: Kind, q: URLSearchParams) {
  const limit = Math.min(500, Math.max(1, Number(q.get('limit') ?? 50)));
  const cursor = q.get('cursor');
  let items = all(kind)
    .filter((o) => matches(kind, o, q))
    .sort((a, b) => (a.metadata.name < b.metadata.name ? -1 : 1));
  if (cursor) items = items.filter((o) => o.metadata.name > cursor);
  const page = items.slice(0, limit);
  return { items: page, ...(items.length > limit ? { nextCursor: page[page.length - 1].metadata.name } : {}) };
}

// ------------------------------------------------------------------ Schreiben

function stored(kind: Kind, name: string, input: Obj, prev?: Obj): Obj {
  const spec = { ...(input.spec ?? {}) };
  validate(kind, name, spec);
  const base = { id: prev?.id ?? uuid(), metadata: meta(name, input.metadata?.labels ?? {}, prev) };
  switch (kind) {
    case 'api-keys':
      return { ...base, spec, status: prev?.status ?? { prefix: rand(12), state: 'active', lastUsedAt: null } };
    case 'provider-credentials': {
      const data = (spec.data ?? {}) as Record<string, unknown>;
      const fields = Object.keys(data).sort();
      checkCredentialFields(spec.provider, fields);
      return { ...base, spec: { provider: spec.provider, fields }, status: { hasSecret: fields.length > 0, keyId: 'a3f1c0de' } };
    }
    case 'deployments':
      return { ...base, spec: { weight: 1, priority: 0, enabled: true, capacity: {}, settings: {}, ...spec } };
    case 'model-groups':
      return { ...base, spec: { aliases: [], fallbacks: [], limits: [], routing: 'weighted', settings: {}, ...spec } };
  }
  return { ...base, spec };
}

function create(kind: Kind, input: Obj): [number, Obj, string] {
  let name = input.metadata?.name;
  if (kind === 'prices' && !name) name = priceName(input.spec?.provider, input.spec?.model ?? '', input.spec?.validFrom ?? '');
  checkName(name);
  if (has(kind, name)) throw conflictP(`„${name}“ existiert bereits`);
  if (kind === 'model-groups' && groupOf(name)) throw conflictP(`„${name}“ ist bereits Alias einer Modellgruppe`);
  const obj = stored(kind, name, input);
  put(kind, obj);
  const e = entry(kind, name);
  if (kind === 'api-keys') return [201, { key: obj, secret: `sk-aigw-${obj.status.prefix}-${rand(43)}` }, etag(kind, name, e)];
  return [201, obj, etag(kind, name, e)];
}

function update(kind: Kind, name: string, input: Obj, headers: Headers): [number, Obj, string] {
  const e = entry(kind, name);
  checkIfMatch(kind, name, e, headers);
  if (input.metadata?.name && input.metadata.name !== name) throw invalid('Umbenennen ist nicht möglich');
  if (kind === 'prices' && new Date(e.obj.spec.validFrom) <= new Date() && JSON.stringify(input.spec) !== JSON.stringify(e.obj.spec)) {
    throw conflictP('Preisversion ist bereits gültig – nur Labels änderbar, neue Version anlegen');
  }
  const obj = stored(kind, name, input, e.obj);
  put(kind, obj);
  return [200, obj, etag(kind, name, entry(kind, name))];
}

function patchCredential(name: string, body: Obj, headers: Headers): [number, Obj, string] {
  const e = entry('provider-credentials', name);
  checkIfMatch('provider-credentials', name, e, headers);
  const fields = new Set<string>(e.obj.spec.fields);
  for (const [k, v] of Object.entries((body.spec?.data ?? {}) as Record<string, unknown>)) {
    if (v === null) fields.delete(k);
    else fields.add(k);
  }
  const provider = body.spec?.provider ?? e.obj.spec.provider;
  if (provider !== e.obj.spec.provider && all('deployments').some((d) => d.spec.credential === name)) throw conflictP('Zugangsdaten werden von Deployments verwendet – Anbieterwechsel nicht möglich');
  const list = [...fields].sort();
  checkCredentialFields(provider, list);
  const obj = {
    ...e.obj,
    metadata: { ...meta(name, body.metadata?.labels ?? e.obj.metadata.labels, e.obj) },
    spec: { provider, fields: list },
    status: { hasSecret: list.length > 0, keyId: 'a3f1c0de' },
  };
  put('provider-credentials', obj);
  return [200, obj, etag('provider-credentials', name, entry('provider-credentials', name))];
}

function keyAction(name: string, verb: string, body: Obj, headers: Headers): [number, Obj, string?] {
  const e = entry('api-keys', name);
  checkIfMatch('api-keys', name, e, headers);
  const k = structuredClone(e.obj);
  if (verb === 'revoke' || verb === 'reactivate') {
    const state = verb === 'revoke' ? 'revoked' : 'active';
    if (k.status.state !== state) {
      k.status.state = state;
      k.metadata.updatedAt = now();
      put('api-keys', k);
    }
    return [200, k, etag('api-keys', name, entry('api-keys', name))];
  }
  if (verb === 'rotate') {
    if (k.status.state === 'revoked') throw conflictP('gesperrte Keys lassen sich nicht rotieren');
    const stamp = now().replace(/[-:]/g, '').replace('Z', '');
    const newName = body.name || `${name.replace(/-\d{8}T\d{6}$/, '')}-${stamp}`.toLowerCase();
    const [, created] = create('api-keys', { metadata: { name: newName, labels: k.metadata.labels }, spec: k.spec });
    const grace = Number(body.gracePeriodSeconds ?? 86_400);
    const until = new Date(Date.now() + grace * 1000).toISOString();
    if (!k.spec.expiresAt || k.spec.expiresAt > until) k.spec.expiresAt = until;
    k.metadata.updatedAt = now();
    put('api-keys', k);
    return [201, { newKey: created, previousKey: k }];
  }
  throw notFound('Aktion', verb);
}

function adminKeys(method: string, name: string | undefined, body: Obj): [number, Obj | null] {
  if (!name) {
    if (method === 'GET') return [200, { items: all('admin-api-keys').sort((a, b) => (a.name < b.name ? -1 : 1)) }];
    checkName(body.name);
    if (has('admin-api-keys', body.name)) throw conflictP(`„${body.name}“ existiert bereits`);
    if (!body.roles?.length) throw invalid('mindestens eine Rolle angeben');
    const t = now();
    const key: S['AdminApiKey'] = { id: uuid(), name: body.name, description: body.description ?? null, prefix: rand(12), roles: body.roles, createdBy: 'user:anna', createdAt: t, updatedAt: t, expiresAt: body.expiresAt ?? null, lastUsedAt: null };
    put('admin-api-keys', key);
    return [201, { key, secret: `sk-aigw-admin-${key.prefix}-${rand(43)}` }];
  }
  const e = entry('admin-api-keys', name);
  if (method === 'GET') return [200, e.obj];
  if (method === 'DELETE') {
    store()['admin-api-keys'].delete(name);
    return [204, null];
  }
  if (!body.roles?.length) throw invalid('mindestens eine Rolle angeben');
  const key = { ...e.obj, description: body.description ?? null, roles: body.roles, expiresAt: body.expiresAt ?? null, updatedAt: now() };
  put('admin-api-keys', key);
  return [200, key];
}

function exportYaml(apiKeys: string | null): string {
  const KIND: Partial<Record<Kind, string>> = {
    organizations: 'Organization',
    teams: 'Team',
    users: 'User',
    'provider-credentials': 'ProviderCredential',
    'model-groups': 'ModelGroup',
    deployments: 'Deployment',
    prices: 'Price',
    'api-keys': 'ApiKey',
  };
  const docs: string[] = [];
  for (const [kind, k] of Object.entries(KIND) as [Kind, string][]) {
    if (kind === 'api-keys' && apiKeys === 'ignore') continue;
    for (const o of all(kind)) {
      const spec = kind === 'provider-credentials' ? { provider: o.spec.provider, data: Object.fromEntries(o.spec.fields.map((f: string) => [f, { valueFrom: { env: `AIGW_SECRET_${o.metadata.name.toUpperCase().replace(/[^A-Z0-9]/g, '_')}_${f.toUpperCase()}` } }])) } : o.spec;
      // JSON ist gültiges YAML (Fluss-Stil) – genügt für den Mock.
      docs.push(`apiVersion: aigw/v1alpha1\nkind: ${k}\nmetadata: ${JSON.stringify({ name: o.metadata.name, labels: o.metadata.labels })}\nspec: ${JSON.stringify(spec)}\n`);
    }
  }
  return `# ai-gw Mock-Export (Beispieldaten, nicht für echte Gateways)\n${docs.join('---\n')}`;
}

function applyPlan(yaml: string, q: URLSearchParams): S['ApplyReport'] {
  const kinds = [...yaml.matchAll(/^kind:\s*(\S+)\s*$/gm)].map((m) => m[1]);
  const names = [...yaml.matchAll(/^metadata:.*?"?name"?:\s*"?([a-z0-9._-]+)"?/gm)].map((m) => m[1]);
  if (!kinds.length) throw invalid('keine Manifeste gefunden (erwartet: Dokumente mit apiVersion/kind/metadata/spec)');
  const changes: S['Change'][] = kinds.map((kind, i) => ({ kind: kind as S['ResourceKind'], name: names[i] ?? `#${i + 1}`, action: 'unchanged', fields: [] }));
  const summary = { create: 0, update: 0, delete: 0, unchanged: changes.length, retain: 0, skip: 0 };
  return {
    applied: false,
    configVersion: 42,
    plan: {
      baseVersion: 42,
      prune: q.get('prune') === 'true',
      apiKeys: (q.get('apiKeys') as S['ApiKeyMode']) ?? 'include',
      changes,
      summary,
      warnings: ['Mock-Modus: Manifeste werden nur grob gelesen und nicht angewendet.'],
    },
  };
}

// ------------------------------------------------------------------ Einstieg

function json(status: number, body: unknown, extra: Record<string, string> = {}): Response {
  if (status === 204 || body === null) return new Response(null, { status: 204 });
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json', 'Cache-Control': 'no-store', ...extra } });
}

export async function mockFetch(req: Request): Promise<Response> {
  const url = new URL(req.url, location.href);
  const parts = url.pathname.replace(/^\/admin\/api\/v1\//, '').split('/').map(decodeURIComponent);
  const text = ['POST', 'PUT', 'PATCH'].includes(req.method) ? await req.text() : '';
  await new Promise((r) => setTimeout(r, 120));
  try {
    if (parts[0] === 'config:export') {
      return new Response(exportYaml(url.searchParams.get('apiKeys')), { status: 200, headers: { 'Content-Type': 'application/yaml' } });
    }
    if (parts[0] === 'config:apply') return json(200, applyPlan(text, url.searchParams));
    const body: Obj = text && parts[0] !== 'config:apply' ? JSON.parse(text) : {};
    const kind = parts[0] as Kind;
    if (!KINDS.includes(kind)) throw notFound('Pfad', url.pathname);
    if (kind === 'admin-api-keys') {
      const [s, b] = adminKeys(req.method, parts[1], body);
      return json(s, b);
    }
    const name = parts[1];
    if (!name) {
      if (req.method === 'GET') return json(200, list(kind, url.searchParams));
      const [s, b, tag] = create(kind, body);
      return json(s, b, { ETag: tag });
    }
    if (parts[2]) {
      const [s, b, tag] = keyAction(name, parts[2], body, req.headers);
      return json(s, b, tag ? { ETag: tag } : {});
    }
    switch (req.method) {
      case 'GET': {
        const e = entry(kind, name);
        return json(200, e.obj, { ETag: etag(kind, name, e) });
      }
      case 'PUT': {
        const [s, b, tag] = update(kind, name, body, req.headers);
        return json(s, b, { ETag: tag });
      }
      case 'PATCH': {
        const [s, b, tag] = patchCredential(name, body, req.headers);
        return json(s, b, { ETag: tag });
      }
      case 'DELETE': {
        const e = entry(kind, name);
        checkIfMatch(kind, name, e, req.headers);
        blockDelete(kind, name);
        store()[kind].delete(name);
        return json(204, null);
      }
    }
    throw new Problem(405, 'method-not-allowed', 'Methode nicht erlaubt');
  } catch (e) {
    const p = e instanceof Problem ? e : new Problem(400, 'bad-request', 'Ungültige Anfrage', e instanceof Error ? e.message : String(e));
    return new Response(JSON.stringify({ type: `urn:aigw:problem:${p.code}`, title: p.title, status: p.status, detail: p.detail ?? null, code: p.code }), {
      status: p.status,
      headers: { 'Content-Type': 'application/problem+json' },
    });
  }
}
