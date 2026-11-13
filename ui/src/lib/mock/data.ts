// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Deterministische Mock-Daten (fester Seed) für die Musterseiten: gleiche Zahlen bei jedem Laden,
// damit Screenshots und Reviews vergleichbar bleiben. Zeitstempel relativ zu „jetzt“.
import type {
  ApiKey,
  Attempt,
  Deployment,
  Kpis,
  LogEntry,
  ModelGroup,
  Org,
  Overview,
  ProviderId,
  RangeId,
  RouteRule,
  Series,
} from './types';

function rng(seed: number) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const pick = <T>(r: () => number, xs: readonly T[]): T => xs[Math.floor(r() * xs.length)];
const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

export const PROVIDERS: Record<ProviderId, { name: string; short: string }> = {
  openai: { name: 'OpenAI', short: 'OAI' },
  azure: { name: 'Azure OpenAI', short: 'AZ' },
  anthropic: { name: 'Anthropic', short: 'ANT' },
  bedrock: { name: 'AWS Bedrock', short: 'AWS' },
  vertex: { name: 'Google Vertex AI', short: 'GCP' },
  mistral: { name: 'Mistral AI', short: 'MIS' },
  vllm: { name: 'vLLM (on-prem)', short: 'VLM' },
};

export const ORGS: Org[] = [
  {
    id: 'acme',
    name: 'Acme GmbH',
    plan: 'Produktion',
    teams: [
      { id: 'platform', name: 'Plattform' },
      { id: 'data', name: 'Data Science' },
      { id: 'support', name: 'Support-Assistent' },
      { id: 'marketing', name: 'Marketing' },
      { id: 'legal', name: 'Recht & Compliance' },
    ],
  },
  { id: 'acme-staging', name: 'Acme Staging', plan: 'Test', teams: [{ id: 'qa', name: 'QA' }] },
  { id: 'labs', name: 'Acme Labs', plan: 'Forschung', teams: [{ id: 'research', name: 'Research' }] },
];

export const teamName = (id: string) => ORGS.flatMap((o) => o.teams).find((t) => t.id === id)?.name ?? id;

// ------------------------------------------------------------------ Modelle & Routen
const dep = (
  id: string,
  provider: ProviderId,
  upstreamModel: string,
  region: string,
  priority: number,
  weight: number,
  health: Deployment['health'],
  p95: number,
  successRate: number,
  rpm: number,
  rpmLimit: number,
): Deployment => ({ id, provider, upstreamModel, region, priority, weight, health, p95, successRate, rpm, rpmLimit });

export const MODEL_GROUPS: ModelGroup[] = [
  {
    alias: 'gpt-4o',
    description: 'Standard-Chatmodell für Produktivlasten, EU-Datenhaltung bevorzugt',
    modality: 'chat',
    strategy: 'priority',
    contextWindow: 128_000,
    priceIn: 2.3,
    priceOut: 9.2,
    requests24h: 412_880,
    deployments: [
      dep('az-swc-gpt4o', 'azure', 'gpt-4o-2024-11-20', 'swedencentral', 1, 60, 'healthy', 1840, 0.998, 1420, 3000),
      dep('az-frc-gpt4o', 'azure', 'gpt-4o-2024-11-20', 'francecentral', 1, 40, 'degraded', 3120, 0.962, 880, 2000),
      dep('oai-gpt4o', 'openai', 'gpt-4o', 'global', 2, 100, 'healthy', 2210, 0.995, 64, 10000),
    ],
    fallbacks: ['claude-sonnet', 'llama-3.3-70b'],
  },
  {
    alias: 'claude-sonnet',
    description: 'Lange Kontexte, Code-Assistenz',
    modality: 'chat',
    strategy: 'priority',
    contextWindow: 200_000,
    priceIn: 2.8,
    priceOut: 13.8,
    requests24h: 268_310,
    deployments: [
      dep('ant-sonnet', 'anthropic', 'claude-sonnet-4-5', 'global', 1, 100, 'healthy', 2480, 0.997, 910, 4000),
      dep('bedrock-fra-sonnet', 'bedrock', 'eu.anthropic.claude-sonnet-4-5', 'eu-central-1', 2, 70, 'healthy', 2710, 0.996, 42, 1500),
      dep('vertex-ew4-sonnet', 'vertex', 'claude-sonnet-4-5', 'europe-west4', 2, 30, 'healthy', 2950, 0.993, 18, 1000),
    ],
    fallbacks: ['gpt-4o'],
  },
  {
    alias: 'gpt-4o-mini',
    description: 'Günstig für Klassifikation und Zusammenfassungen',
    modality: 'chat',
    strategy: 'cost',
    contextWindow: 128_000,
    priceIn: 0.14,
    priceOut: 0.55,
    requests24h: 902_440,
    deployments: [
      dep('az-swc-mini', 'azure', 'gpt-4o-mini', 'swedencentral', 1, 50, 'healthy', 720, 0.999, 2210, 6000),
      dep('oai-mini', 'openai', 'gpt-4o-mini', 'global', 1, 50, 'healthy', 810, 0.998, 1980, 30000),
    ],
    fallbacks: ['llama-3.3-70b'],
  },
  {
    alias: 'llama-3.3-70b',
    description: 'Selbst betrieben, keine Daten an Dritte',
    modality: 'chat',
    strategy: 'latency',
    contextWindow: 128_000,
    priceIn: 0.0,
    priceOut: 0.0,
    requests24h: 154_120,
    deployments: [
      dep('vllm-rz1-a', 'vllm', 'meta-llama/Llama-3.3-70B-Instruct', 'rz-nord', 1, 50, 'healthy', 1320, 0.994, 380, 600),
      dep('vllm-rz1-b', 'vllm', 'meta-llama/Llama-3.3-70B-Instruct', 'rz-nord', 1, 50, 'down', 0, 0.0, 0, 600),
      dep('mistral-large-fb', 'mistral', 'mistral-large-latest', 'eu', 2, 100, 'healthy', 1910, 0.996, 12, 1200),
    ],
    fallbacks: ['gpt-4o-mini'],
  },
  {
    alias: 'text-embedding-3-large',
    description: 'Embeddings für RAG und semantischen Cache',
    modality: 'embedding',
    strategy: 'weighted',
    contextWindow: 8_191,
    priceIn: 0.12,
    priceOut: 0,
    requests24h: 1_288_400,
    deployments: [
      dep('az-swc-emb', 'azure', 'text-embedding-3-large', 'swedencentral', 1, 80, 'healthy', 140, 0.9995, 4100, 12000),
      dep('oai-emb', 'openai', 'text-embedding-3-large', 'global', 1, 20, 'healthy', 210, 0.999, 1020, 20000),
    ],
    fallbacks: [],
  },
];

export const ROUTES: RouteRule[] = [
  { id: 'r1', match: 'gpt-4o', target: 'gpt-4o', teams: 'all', note: 'Direkter Alias', enabled: true },
  { id: 'r2', match: 'gpt-4*', target: 'gpt-4o', teams: 'all', note: 'Altnamen (gpt-4, gpt-4-turbo) umleiten', enabled: true },
  { id: 'r3', match: 'claude-*', target: 'claude-sonnet', teams: ['platform', 'data'], note: 'Anthropic-Eingang (Messages API)', enabled: true },
  { id: 'r4', match: 'gpt-4o-mini', target: 'gpt-4o-mini', teams: 'all', note: 'Direkter Alias', enabled: true },
  { id: 'r5', match: 'local/*', target: 'llama-3.3-70b', teams: ['legal'], note: 'Vertrauliche Daten nur on-prem', enabled: true },
  { id: 'r6', match: 'text-embedding-*', target: 'text-embedding-3-large', teams: 'all', note: 'Embeddings vereinheitlichen', enabled: true },
  { id: 'r7', match: 'o3*', target: 'gpt-4o', teams: ['data'], note: 'Test: Reasoning-Modelle', enabled: false },
];

// ------------------------------------------------------------------ Keys
const KEY_NAMES: [string, string][] = [
  ['support-bot-prod', 'support'],
  ['support-bot-staging', 'support'],
  ['data-notebooks', 'data'],
  ['rag-indexer', 'data'],
  ['ci-evals', 'platform'],
  ['platform-gateway-probe', 'platform'],
  ['marketing-copy', 'marketing'],
  ['seo-generator', 'marketing'],
  ['contract-review', 'legal'],
  ['legacy-intranet', 'platform'],
  ['hackathon-2026', 'data'],
  ['jupyter-hub', 'data'],
  ['translation-service', 'support'],
  ['newsletter-ai', 'marketing'],
];
const PEOPLE = ['a.becker', 'm.yilmaz', 'j.schulz', 's.nowak', 'l.fischer'];

function mkKeys(now: number): ApiKey[] {
  const r = rng(48);
  return KEY_NAMES.map(([name, teamId], i) => {
    const created = now - Math.floor(10 + r() * 300) * DAY;
    const status: ApiKey['status'] = i === 9 ? 'revoked' : i === 10 ? 'expired' : 'active';
    const limit = r() < 0.2 ? null : pick(r, [50, 100, 250, 500, 1000, 2500]);
    const used = limit ? Math.round(limit * (0.08 + r() * (i === 0 ? 0.9 : 0.75)) * 100) / 100 : Math.round(r() * 900 * 100) / 100;
    const models: ApiKey['models'] =
      r() < 0.35 ? 'all' : MODEL_GROUPS.filter(() => r() < 0.45).map((m) => m.alias).slice(0, 3);
    return {
      id: `key_${(0x1a2b3c + i * 7919).toString(16)}`,
      name,
      prefix: 'sk-gw-',
      last4: Math.floor(r() * 0xffff)
        .toString(16)
        .padStart(4, '0'),
      teamId,
      models: Array.isArray(models) && models.length === 0 ? ['gpt-4o-mini'] : models,
      budgetLimit: limit,
      budgetUsed: used,
      rpm: r() < 0.25 ? null : pick(r, [60, 120, 300, 600, 1200]),
      tpm: r() < 0.3 ? null : pick(r, [50_000, 100_000, 250_000, 1_000_000]),
      status,
      createdAt: created,
      createdBy: pick(r, PEOPLE),
      lastUsedAt: status === 'active' ? now - Math.floor(r() * (i === 5 ? 50 : 4000)) * MIN : created + 20 * DAY,
      expiresAt: i === 10 ? now - 3 * DAY : r() < 0.4 ? now + Math.floor(5 + r() * 200) * DAY : null,
      requests30d: Math.floor(r() * 900_000),
    };
  });
}

// ------------------------------------------------------------------ Zeitreihen
const RANGES: Record<RangeId, { points: number; step: number }> = {
  '24h': { points: 96, step: 15 * MIN },
  '7d': { points: 168, step: HOUR },
  '30d': { points: 120, step: 6 * HOUR },
};

function mkSeries(range: RangeId, now: number, seed: number): Series {
  const { points, step } = RANGES[range];
  const r = rng(seed);
  const end = Math.floor(now / step) * step;
  const s: Series = { t: [], requests: [], tokens: [], cost: [], p95: [], errorRate: [], cacheRate: [] };
  for (let i = 0; i < points; i++) {
    const t = end - (points - 1 - i) * step;
    const h = new Date(t).getHours() + new Date(t).getMinutes() / 60;
    const dow = new Date(t).getDay();
    const diurnal = 0.35 + 0.65 * Math.max(0, Math.sin(((h - 6) / 16) * Math.PI));
    const weekday = dow === 0 || dow === 6 ? 0.45 : 1;
    const trend = 1 + (i / points) * 0.12;
    const spike = range === '24h' && i > 60 && i < 66 ? 1.6 : 1;
    const perMin = 2600 * diurnal * weekday * trend * spike * (0.9 + r() * 0.2);
    const req = Math.round((perMin * step) / MIN);
    const tok = Math.round(req * (880 + r() * 160));
    const incident = range === '24h' && i > 61 && i < 65;
    s.t.push(t / 1000);
    s.requests.push(req);
    s.tokens.push(tok);
    s.cost.push(Math.round(tok * 0.0000031 * 100) / 100);
    s.p95.push(Math.round((1350 + diurnal * 650 + r() * 220) * (incident ? 2.1 : 1)));
    s.errorRate.push(Math.round((0.004 + r() * 0.004 + (incident ? 0.035 : 0)) * 10000) / 10000);
    s.cacheRate.push(Math.round((0.27 + r() * 0.06 - diurnal * 0.04) * 1000) / 1000);
  }
  return s;
}

function aggregate(s: Series): Kpis {
  const sum = (xs: number[]) => xs.reduce((a, b) => a + b, 0);
  const req = sum(s.requests);
  const w = (xs: number[]) => s.requests.reduce((a, n, i) => a + n * xs[i], 0) / (req || 1);
  const sorted = [...s.p95].sort((a, b) => a - b);
  return {
    requests: req,
    tokens: sum(s.tokens),
    cost: Math.round(sum(s.cost) * 100) / 100,
    p95: sorted[Math.floor(sorted.length * 0.9)] ?? 0,
    errorRate: w(s.errorRate),
    cacheRate: w(s.cacheRate),
  };
}

// ------------------------------------------------------------------ Logs
const PROMPTS = [
  'Fasse das folgende Support-Ticket in zwei Sätzen zusammen: Kunde meldet, dass der Export nach dem Update …',
  'Klassifiziere die Anfrage in eine der Kategorien Rechnung, Technik, Vertrag, Sonstiges.',
  'Schreibe drei Varianten für eine Betreffzeile zum Herbst-Newsletter.',
  'Übersetze ins Englische: „Ihre Bestellung wurde versandt und trifft voraussichtlich morgen ein.“',
  'Prüfe die Klausel auf ungewöhnliche Haftungsbeschränkungen und nenne die Fundstelle.',
  'Erkläre den Unterschied zwischen Prioritäts- und gewichtetem Routing in einem Absatz.',
];
const ANSWERS = [
  'Der Kunde kann seit dem Update 4.2 keine CSV-Exporte mehr erstellen; der Fehler tritt nur bei Zeiträumen über 90 Tagen auf.',
  'Kategorie: Technik',
  '1. Goldener Herbst, frische Ideen  2. Was dieser Oktober für Sie bereithält  3. Drei Neuheiten, die Sie kennen sollten',
  'Your order has been shipped and is expected to arrive tomorrow.',
  'Abschnitt 9.3 begrenzt die Haftung auch für grobe Fahrlässigkeit – das ist unüblich und sollte nachverhandelt werden.',
  'Beim Prioritätsrouting wird immer die höchste verfügbare Gruppe genutzt; gewichtetes Routing verteilt die Last anteilig.',
];

function mkLogs(now: number, keys: ApiKey[]): LogEntry[] {
  const r = rng(4242);
  const active = keys.filter((k) => k.status === 'active');
  const out: LogEntry[] = [];
  let t = now - 4000;
  for (let i = 0; i < 260; i++) {
    t -= Math.floor(4000 + r() * 26000);
    const group = pick(r, [MODEL_GROUPS[0], MODEL_GROUPS[0], MODEL_GROUPS[1], MODEL_GROUPS[2], MODEL_GROUPS[2], MODEL_GROUPS[3], MODEL_GROUPS[4]]);
    const key = pick(r, active);
    const roll = r();
    const status = roll < 0.035 ? 429 : roll < 0.05 ? 502 : roll < 0.058 ? 400 : 200;
    const healthy = group.deployments.filter((d) => d.health !== 'down');
    const first = healthy[0];
    const attempts: Attempt[] = [];
    let final = first;
    if (roll > 0.92 && group.deployments.length > 1 && status === 200) {
      attempts.push({ deployment: first.id, provider: first.provider, status: 503, latency: 3000 + Math.floor(r() * 2000), error: 'upstream: Service Unavailable' });
      final = group.deployments[group.deployments.length - 1];
    }
    const embedding = group.modality === 'embedding';
    const inTok = embedding ? Math.floor(200 + r() * 1800) : Math.floor(120 + r() * 3200);
    const outTok = embedding || status !== 200 ? 0 : Math.floor(20 + r() * 900);
    const cache: LogEntry['cache'] = status !== 200 ? 'miss' : r() < 0.2 ? 'hit' : r() < 0.08 ? 'semantic' : 'miss';
    const base = cache === 'miss' ? final.p95 * (0.35 + r() * 0.7) : 8 + r() * 30;
    const latency = Math.round(base + attempts.reduce((a, x) => a + x.latency, 0));
    attempts.push({ deployment: final.id, provider: final.provider, status, latency: Math.round(base), error: status === 200 ? undefined : status === 429 ? 'rate_limit_exceeded' : status === 502 ? 'upstream: Bad Gateway' : 'invalid_request_error' });
    const anthropicIn = group.alias === 'claude-sonnet' && r() < 0.6;
    const p = Math.floor(r() * PROMPTS.length);
    const id = `req_${(0x5f3a00 + i * 104729).toString(36)}${Math.floor(r() * 1e6).toString(36)}`;
    const cost = cache === 'hit' ? 0 : Math.round(((inTok * group.priceIn + outTok * group.priceOut) / 1e6) * 1e6) / 1e6;
    const request = embedding
      ? { model: group.alias, input: [PROMPTS[p]], encoding_format: 'float' }
      : anthropicIn
        ? { model: 'claude-sonnet-4-5', max_tokens: 1024, system: 'Du bist ein hilfreicher Assistent der Acme GmbH.', messages: [{ role: 'user', content: PROMPTS[p] }] }
        : { model: group.alias, temperature: 0.2, stream: r() < 0.4, messages: [{ role: 'system', content: 'Du bist ein hilfreicher Assistent der Acme GmbH.' }, { role: 'user', content: PROMPTS[p] }], metadata: { team: key.teamId } };
    const response =
      status !== 200
        ? { error: { type: attempts[attempts.length - 1].error, message: status === 429 ? `Ratenlimit für Key ${key.name} erreicht (${key.rpm ?? 600} RPM)` : 'Upstream-Fehler nach allen Fallbacks', code: status } }
        : embedding
          ? { object: 'list', model: final.upstreamModel, data: [{ object: 'embedding', index: 0, embedding: '[3072 Werte]' }], usage: { prompt_tokens: inTok, total_tokens: inTok } }
          : anthropicIn
            ? { id: `msg_${id.slice(4)}`, type: 'message', role: 'assistant', model: final.upstreamModel, content: [{ type: 'text', text: ANSWERS[p] }], stop_reason: 'end_turn', usage: { input_tokens: inTok, output_tokens: outTok } }
            : { id: `chatcmpl-${id.slice(4)}`, object: 'chat.completion', model: final.upstreamModel, choices: [{ index: 0, message: { role: 'assistant', content: ANSWERS[p] }, finish_reason: 'stop' }], usage: { prompt_tokens: inTok, completion_tokens: outTok, total_tokens: inTok + outTok } };
    const upstream = Math.max(0, latency - 14);
    out.push({
      id,
      ts: t,
      status,
      endpoint: embedding ? '/v1/embeddings' : anthropicIn ? '/v1/messages' : '/v1/chat/completions',
      inputFormat: anthropicIn ? 'anthropic' : 'openai',
      model: group.alias,
      provider: final.provider,
      deployment: final.id,
      keyName: key.name,
      teamId: key.teamId,
      inTok,
      outTok,
      cost,
      latency,
      ttfb: Math.round(cache === 'miss' ? latency * (0.25 + r() * 0.2) : latency),
      cache,
      stream: !embedding && r() < 0.4,
      attempts,
      request,
      response,
      stages:
        cache === 'miss'
          ? [
              { name: 'Auth & Key-Prüfung', ms: 1.2 },
              { name: 'Limits & Budget', ms: 0.6 },
              { name: 'Guardrails (Eingang)', ms: 6.4 },
              { name: 'Übersetzung → IR → Anbieter', ms: 0.3 },
              { name: 'Upstream', ms: upstream },
              { name: 'Guardrails (Ausgang)', ms: 4.9 },
              { name: 'Logging & Abrechnung', ms: 0.4 },
            ]
          : [
              { name: 'Auth & Key-Prüfung', ms: 1.2 },
              { name: 'Limits & Budget', ms: 0.6 },
              { name: cache === 'semantic' ? 'Semantischer Cache (Vektor)' : 'Exakter Cache', ms: Math.max(1, latency - 3) },
              { name: 'Logging & Abrechnung', ms: 0.4 },
            ],
    });
  }
  return out;
}

// ------------------------------------------------------------------ Fassade
const NOW = Date.now();
export const MOCK = (() => {
  const keys = mkKeys(NOW);
  return { now: NOW, keys, logs: mkLogs(NOW, keys) };
})();

export function overview(range: RangeId): Overview {
  const seed = { '24h': 1, '7d': 2, '30d': 3 }[range];
  const series = mkSeries(range, NOW, 100 + seed);
  const prevSeries = mkSeries(range, NOW - RANGES[range].points * RANGES[range].step, 200 + seed);
  const kpis = aggregate(series);
  const previous = aggregate(prevSeries);
  previous.requests = Math.round(previous.requests * 0.91);
  previous.tokens = Math.round(previous.tokens * 0.88);
  previous.cost = Math.round(previous.cost * 0.9 * 100) / 100;
  previous.p95 = Math.round(previous.p95 * 0.94);
  previous.errorRate = previous.errorRate * 1.25;
  previous.cacheRate = previous.cacheRate * 0.93;
  const total = MODEL_GROUPS.reduce((a, m) => a + m.requests24h, 0);
  const factor = kpis.requests / total;
  return {
    range,
    series,
    kpis,
    previous,
    topModels: MODEL_GROUPS.map((m, i) => {
      const req = Math.round(m.requests24h * factor);
      const tok = m.modality === 'embedding' ? req * 600 : req * 1100;
      return {
        alias: m.alias,
        requests: req,
        tokens: tok,
        cost: Math.round(((tok * 0.7 * m.priceIn + tok * 0.3 * m.priceOut) / 1e6) * 100) / 100,
        p95: Math.max(...m.deployments.map((d) => d.p95)) - i * 40,
        errorRate: 1 - Math.min(...m.deployments.filter((d) => d.health !== 'down').map((d) => d.successRate)),
      };
    }).sort((a, b) => b.requests - a.requests),
    providers: [
      { id: 'azure', health: 'degraded', p95: 2140, errorRate: 0.012, share: 0.46 },
      { id: 'openai', health: 'healthy', p95: 1720, errorRate: 0.004, share: 0.21 },
      { id: 'anthropic', health: 'healthy', p95: 2480, errorRate: 0.003, share: 0.17 },
      { id: 'vllm', health: 'degraded', p95: 1320, errorRate: 0.006, share: 0.09 },
      { id: 'bedrock', health: 'healthy', p95: 2710, errorRate: 0.004, share: 0.04 },
      { id: 'mistral', health: 'healthy', p95: 1910, errorRate: 0.004, share: 0.02 },
      { id: 'vertex', health: 'healthy', p95: 2950, errorRate: 0.007, share: 0.01 },
    ],
  };
}

/** Neuer Key: Klartext wird genau einmal zurückgegeben (wie später die Admin-API). */
export function createKey(input: {
  name: string;
  teamId: string;
  models: string[] | 'all';
  budgetLimit: number | null;
  rpm: number | null;
  tpm: number | null;
  expiresInDays: number | null;
}): { key: ApiKey; secret: string } {
  const bytes = new Uint8Array(24);
  crypto.getRandomValues(bytes);
  const body = Array.from(bytes, (b) => 'abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789'[b % 57]).join('');
  const secret = `sk-gw-${body}`;
  const now = Date.now();
  const key: ApiKey = {
    id: `key_${now.toString(16)}`,
    name: input.name,
    prefix: 'sk-gw-',
    last4: secret.slice(-4),
    teamId: input.teamId,
    models: input.models,
    budgetLimit: input.budgetLimit,
    budgetUsed: 0,
    rpm: input.rpm,
    tpm: input.tpm,
    status: 'active',
    createdAt: now,
    createdBy: 'a.becker',
    lastUsedAt: null,
    expiresAt: input.expiresInDays ? now + input.expiresInDays * DAY : null,
    requests30d: 0,
  };
  return { key, secret };
}
