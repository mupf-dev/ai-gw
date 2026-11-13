// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Datentypen der Musterseiten. Vorläufig (UI-seitig); mit den Admin-API-Endpunkten ersetzt durch die aus
// der OpenAPI-Spezifikation generierten Typen (src/lib/api/schema.d.ts).

export type ProviderId = 'openai' | 'azure' | 'anthropic' | 'bedrock' | 'vertex' | 'mistral' | 'vllm';
export type Health = 'healthy' | 'degraded' | 'down';

export interface Team {
  id: string;
  name: string;
}
export interface Org {
  id: string;
  name: string;
  plan: string;
  teams: Team[];
}

export interface ApiKey {
  id: string;
  name: string;
  prefix: string;
  last4: string;
  teamId: string;
  models: string[] | 'all';
  budgetLimit: number | null;
  budgetUsed: number;
  rpm: number | null;
  tpm: number | null;
  status: 'active' | 'revoked' | 'expired';
  createdAt: number;
  createdBy: string;
  lastUsedAt: number | null;
  expiresAt: number | null;
  requests30d: number;
}

export interface Deployment {
  id: string;
  provider: ProviderId;
  upstreamModel: string;
  region: string;
  priority: number;
  weight: number;
  health: Health;
  p95: number;
  successRate: number;
  rpm: number;
  rpmLimit: number;
}

export type Strategy = 'priority' | 'weighted' | 'latency' | 'cost';

export interface ModelGroup {
  alias: string;
  description: string;
  modality: 'chat' | 'embedding';
  strategy: Strategy;
  contextWindow: number;
  priceIn: number;
  priceOut: number;
  requests24h: number;
  deployments: Deployment[];
  fallbacks: string[];
}

export interface RouteRule {
  id: string;
  match: string;
  target: string;
  teams: string[] | 'all';
  note: string;
  enabled: boolean;
}

export interface Attempt {
  deployment: string;
  provider: ProviderId;
  status: number;
  latency: number;
  error?: string;
}

export interface LogEntry {
  id: string;
  ts: number;
  status: number;
  endpoint: string;
  inputFormat: 'openai' | 'anthropic';
  model: string;
  provider: ProviderId;
  deployment: string;
  keyName: string;
  teamId: string;
  inTok: number;
  outTok: number;
  cost: number;
  latency: number;
  ttfb: number;
  cache: 'hit' | 'semantic' | 'miss';
  stream: boolean;
  attempts: Attempt[];
  request: unknown;
  response: unknown;
  stages: { name: string; ms: number }[];
}

export type RangeId = '24h' | '7d' | '30d';

export interface Series {
  t: number[];
  requests: number[];
  tokens: number[];
  cost: number[];
  p95: number[];
  errorRate: number[];
  cacheRate: number[];
}

export interface Kpis {
  requests: number;
  tokens: number;
  cost: number;
  p95: number;
  errorRate: number;
  cacheRate: number;
}

export interface Overview {
  range: RangeId;
  series: Series;
  kpis: Kpis;
  previous: Kpis;
  topModels: { alias: string; requests: number; tokens: number; cost: number; p95: number; errorRate: number }[];
  providers: { id: ProviderId; health: Health; p95: number; errorRate: number; share: number }[];
}
