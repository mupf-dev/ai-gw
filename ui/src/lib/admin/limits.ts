// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// Beschriftungen für Limits (LimitSpec) und Budgets: Kennzahlen, Fenster (gleitend bzw. Kalender, F25).
import type { S } from '../api/resources';

export const METRICS: { value: S['LimitMetric']; label: string }[] = [
  { value: 'requests', label: 'Anfragen' },
  { value: 'tokens', label: 'Tokens' },
  { value: 'input-tokens', label: 'Eingabe-Tokens' },
  { value: 'output-tokens', label: 'Ausgabe-Tokens' },
  { value: 'concurrency', label: 'gleichzeitige Anfragen' },
];
export const WINDOWS: { value: S['LimitWindow']; label: string; hint: string }[] = [
  { value: 'second', label: 'Sekunde', hint: 'gleitend; mit postgres-sync ungenau' },
  { value: 'minute', label: 'Minute', hint: 'gleitend' },
  { value: 'hour', label: 'Stunde', hint: 'gleitend' },
  { value: 'day', label: 'Tag (24 Std.)', hint: 'gleitend' },
  { value: '30d', label: '30 Tage', hint: 'gleitend' },
  { value: 'calendar-day', label: 'Kalendertag', hint: 'ab 0 Uhr (billing.timezone)' },
  { value: 'calendar-week', label: 'Kalenderwoche', hint: 'ab Montag' },
  { value: 'calendar-month', label: 'Kalendermonat', hint: 'ab dem 1.' },
];
export const metricLabel = (m: string) => METRICS.find((x) => x.value === m)?.label ?? m;
export const windowLabel = (w: string | undefined) => WINDOWS.find((x) => x.value === (w ?? 'minute'))?.label ?? w ?? '';
export const PERIOD: Record<S['BudgetPeriod'], string> = { day: 'Tag', week: 'Woche', month: 'Monat', total: 'gesamt' };

/** Zeile im Limit-Editor (Grenze als Text, leere Modellgruppe = alle). */
export interface LimitRow {
  metric: S['LimitMetric'];
  window: S['LimitWindow'];
  limit: string;
  modelGroup: string;
}
export const toRows = (l: S['LimitSpec'][] | undefined): LimitRow[] =>
  (l ?? []).map((x) => ({ metric: x.metric, window: x.window ?? 'minute', limit: String(x.limit), modelGroup: x.modelGroup ?? '' }));
/** Zeilen → LimitSpec[]; `null` bei ungültiger Grenze. */
export function fromRows(rows: LimitRow[]): S['LimitSpec'][] | null {
  const out: S['LimitSpec'][] = [];
  for (const r of rows) {
    if (!r.limit.toString().trim()) continue;
    const n = Number(r.limit);
    if (!Number.isInteger(n) || n < 1) return null;
    out.push({ metric: r.metric, window: r.window, limit: n, ...(r.modelGroup ? { modelGroup: r.modelGroup } : {}) });
  }
  return out;
}
