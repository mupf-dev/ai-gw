// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// Lesende Auswertungs-Endpunkte der Admin-API (AIGW-77): Audit-Log (AIGW-66), Dashboards und Log-Explorer
// (AIGW-46), Budgetstände (AIGW-30/31), Plugin-Bindungen (F148, nur lesen) und Einladungen (F90).
// Docs: docs/konzept/08-admin-api.md §5.10/§5.11.
import { api, listPage, unwrap, type Page, type Query, type Schemas } from './client';

type S = Schemas;
export type AuditEntry = S['AuditEntry'];
export type Summary = S['AnalyticsSummary'];
export type Kpi = S['Kpi'];
export type Timeseries = S['Timeseries'];
export type TopList = S['TopList'];
export type RequestLogItem = S['RequestLogItem'];
export type RequestLogDetail = S['RequestLogDetail'];
export type BudgetStatus = S['BudgetStatus'];
export type PluginBinding = S['PluginBinding'];
export type Invitation = S['Invitation'];
export type Metric = S['Metric'];
export type Dimension = S['Dimension'];

/** Filter der Analytics-Routen (Name oder ID, kommagetrennt). */
export interface ScopeFilter {
  modelGroup?: string;
  deployment?: string;
  provider?: string;
  team?: string;
  key?: string;
  organization?: string;
  /** Fehlerklassen (F47). */
  errorClass?: string;
}

const clean = (q: Record<string, unknown>) => Object.fromEntries(Object.entries(q).filter(([, v]) => v !== undefined && v !== null && v !== ''));

// ------------------------------------------------------------------ Audit-Log

export interface AuditQuery extends Query {
  from?: string;
  to?: string;
  actor?: string;
  actorType?: string;
  kind?: string;
  action?: string;
  object?: string;
  order?: 'asc' | 'desc';
}

export const auditLog = (q: AuditQuery, limit = 50, cursor?: string): Promise<Page<AuditEntry>> => listPage<AuditEntry>('/admin/api/v1/audit-log', q, limit, cursor);

/** NDJSON-Export (alle passenden Einträge, `limit` = Höchstzahl) als Text. */
export async function auditExport(q: AuditQuery, max?: number): Promise<string> {
  return unwrap<string>(
    api.GET('/admin/api/v1/audit-log', {
      params: { query: clean({ ...q, limit: max }) },
      headers: { Accept: 'application/x-ndjson' },
      parseAs: 'text',
    }),
  );
}

// ------------------------------------------------------------------ Analytics

export interface Range {
  from: string;
  to: string;
}

/** Zeitraum relativ zu jetzt (Stunden), auf volle Minuten gerundet (stabile Abfragen). */
export function lastHours(h: number): Range {
  const to = Math.ceil(Date.now() / 60_000) * 60_000;
  return { from: new Date(to - h * 3_600_000).toISOString(), to: new Date(to).toISOString() };
}

export const summary = (r: Range, f: ScopeFilter = {}) =>
  unwrap<Summary>(api.GET('/admin/api/v1/analytics/summary', { params: { query: clean({ ...r, ...f }) as never } }));

export const timeseries = (metric: Metric, r: Range, f: ScopeFilter = {}, opts: { groupBy?: Dimension; step?: S['Step']; limit?: number } = {}) =>
  unwrap<Timeseries>(api.GET('/admin/api/v1/analytics/timeseries', { params: { query: clean({ metric, ...r, ...f, ...opts }) as never } }));

export const top = (dimension: Dimension, r: Range, f: ScopeFilter = {}, opts: { metric?: Metric; limit?: number } = {}) =>
  unwrap<TopList>(api.GET('/admin/api/v1/analytics/top', { params: { query: clean({ dimension, ...r, ...f, ...opts }) as never } }));

// ------------------------------------------------------------------ Log-Explorer

export interface RequestLogQuery extends Query {
  from?: string;
  to?: string;
  status?: string;
  errorCode?: string;
  modelGroup?: string;
  deployment?: string;
  provider?: string;
  team?: string;
  key?: string;
  organization?: string;
  requestId?: string;
  clientRequestId?: string;
  stream?: boolean;
  cache?: string;
  errorClass?: string;
  traceId?: string;
  parentRequestId?: string;
}

export const requestLogs = (q: RequestLogQuery, limit = 50, cursor?: string) => listPage<RequestLogItem>('/admin/api/v1/request-logs', q, limit, cursor);
export const requestLog = (id: string) => unwrap<RequestLogDetail>(api.GET('/admin/api/v1/request-logs/{id}', { params: { path: { id } } }));

// ------------------------------------------------------------------ Budgets

export const budgets = (level?: S['BudgetLevelName']) => unwrap<S['BudgetList']>(api.GET('/admin/api/v1/usage/budgets', { params: { query: clean({ level }) } }));

/** Hinweise zu ungenau wirkenden Limits (F29: Sekundenlimits mit postgres-sync). */
export const limitWarnings = () => unwrap<S['LimitWarnings']>(api.GET('/admin/api/v1/usage/limit-warnings'));

// ------------------------------------------------------------------ Kostenanalyse (AIGW-117)

export type CostSummary = S['CostSummary'];
export type CostDrivers = S['CostDrivers'];
export type CostDriver = S['CostDriver'];
export type BudgetForecast = S['BudgetForecast'];
export type BudgetForecastList = S['BudgetForecastList'];
export type ExportFormat = S['ExportFormat'];
export type Granularity = S['Granularity'];

export const costSummary = (r: Range, f: ScopeFilter = {}) =>
  unwrap<CostSummary>(api.GET('/admin/api/v1/costs/summary', { params: { query: clean({ ...r, ...f, errorClass: undefined }) as never } }));

export const costDrivers = (dimension: Dimension, r: Range, f: ScopeFilter = {}, limit = 10) =>
  unwrap<CostDrivers>(api.GET('/admin/api/v1/costs/breakdown', { params: { query: clean({ dimension, ...r, ...f, errorClass: undefined, limit }) as never } }));

export const budgetForecasts = (q: { level?: S['BudgetLevelName']; target?: string; method?: S['ForecastMethod']; lookbackDays?: number } = {}) =>
  unwrap<BudgetForecastList>(api.GET('/admin/api/v1/costs/budgets', { params: { query: clean(q) as never } }));

/** Chargeback-Export als Text (CSV bzw. JSON); der Server streamt, die UI lädt die Datei als Ganzes. */
export async function costExport(q: { dimension: Dimension; format: ExportFormat; granularity: Granularity } & Range & ScopeFilter): Promise<string> {
  return unwrap<string>(api.GET('/admin/api/v1/costs/export', { params: { query: clean({ ...q, errorClass: undefined }) as never }, parseAs: 'text' }));
}

// ------------------------------------------------------------------ Plugin-Bindungen (lesen, F148)

export const pluginBindings = (level: S['ScopeLevel'], target?: string) =>
  listPage<PluginBinding>('/admin/api/v1/plugin-bindings', { level, target }, 100);

// ------------------------------------------------------------------ Einladungen (F90)

export const invitations = {
  get: (user: string) => unwrap<Invitation>(api.GET('/admin/api/v1/users/{name}/invitation', { params: { path: { name: user } } })),
  create: (user: string) => unwrap<S['CreatedInvitation']>(api.POST('/admin/api/v1/users/{name}/invitation', { params: { path: { name: user } } })),
  revoke: (user: string) => unwrap<void>(api.DELETE('/admin/api/v1/users/{name}/invitation', { params: { path: { name: user } } })),
};

// ------------------------------------------------------------------ Anzeige

/** Betrag (Dezimaltext oder Zahl) ohne Währungssymbol – die Abrechnungswährung gilt je Installation. */
export function amount(v: string | number | null | undefined, digits = 2): string {
  const n = typeof v === 'string' ? Number(v) : (v ?? 0);
  if (!Number.isFinite(n)) return String(v);
  const d = n !== 0 && Math.abs(n) < 0.01 ? 4 : digits;
  return n.toLocaleString('de-DE', { minimumFractionDigits: d, maximumFractionDigits: d });
}
