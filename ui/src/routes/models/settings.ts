// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Strukturierte Routing-Einstellungen (D9/F137): Beschreibung der bekannten Felder in `settings` von Modellgruppen
// und Deployments (docs/konzept/10-routing.md §2/§2b, 06 §3). Unbekannte Schlüssel bleiben erhalten und sind in der
// JSON-Ansicht „Erweitert“ sichtbar; geprüft wird immer auf dem Server (GroupPolicy::from_settings, 422).

export type Kind = 'number' | 'bool' | 'choice' | 'list';
export interface SettingField {
  path: string;
  label: string;
  kind: Kind;
  hint?: string;
  /** Standardwert (nur Anzeige als Platzhalter bzw. „Standard (…)“). */
  def?: string;
  choices?: { value: string; label: string }[];
  /** Nur zeigen, wenn diese Bedingung erfüllt ist. */
  when?: (s: Record<string, unknown>) => boolean;
  min?: number;
  max?: number;
  /** Dezimalzahl erlaubt (sonst ganze Zahl). */
  decimal?: boolean;
}
export interface SettingSection {
  id: string;
  title: string;
  description: string;
  fields: SettingField[];
}

const TIMEOUTS: SettingSection = {
  id: 'timeouts',
  title: 'Zeitlimits',
  description: 'Je Versuch; ohne Angabe gelten die Standardwerte des Anbieters.',
  fields: [
    { path: 'timeouts.connectMs', label: 'Verbindung (ms)', kind: 'number', min: 1 },
    { path: 'timeouts.firstByteMs', label: 'Erstes Byte (ms)', kind: 'number', min: 1 },
    { path: 'timeouts.idleMs', label: 'Pause im Stream (ms)', kind: 'number', min: 1 },
    { path: 'timeouts.totalMs', label: 'Gesamt (ms)', kind: 'number', min: 1 },
  ],
};

const THINKING: SettingField = {
  path: 'thinkingBudgetPolicy',
  label: 'Thinking-Budget (Anthropic)',
  kind: 'choice',
  hint: 'Wenn max_tokens ≤ Budget: max_tokens erhöhen oder Budget kürzen (F59).',
  choices: [
    { value: 'raise-max-tokens', label: 'max_tokens erhöhen' },
    { value: 'cap-budget', label: 'Budget kürzen' },
  ],
  def: 'raise-max-tokens',
};
const TRACE: SettingField = { path: 'propagateTraceContext', label: 'Trace-Kontext weitergeben', kind: 'bool', hint: 'traceparent an den Anbieter (Standard je Anbieterart: intern an, Cloud aus).' };

export const GROUP_SECTIONS: SettingSection[] = [
  {
    id: 'retry',
    title: 'Wiederholungen',
    description: 'Neue Versuche bei retrybaren Fehlern (inkl. des ersten Versuchs), mit exponentiellem Backoff.',
    fields: [
      { path: 'retry.maxAttempts', label: 'Versuche', kind: 'number', min: 1, max: 20, def: '3' },
      { path: 'retry.initialBackoffMs', label: 'Erster Backoff (ms)', kind: 'number', min: 0, def: '100' },
      { path: 'retry.maxBackoffMs', label: 'Höchster Backoff (ms)', kind: 'number', min: 0, def: '2000' },
      { path: 'retry.budgetMs', label: 'Zeitbudget (ms)', kind: 'number', min: 0, def: '30000', hint: 'für alle neuen Versuche inkl. Fallbacks' },
      { path: 'retry.maxRetryAfterMs', label: 'Retry-After höchstens (ms)', kind: 'number', min: 0, def: '10000' },
    ],
  },
  {
    id: 'circuitBreaker',
    title: 'Circuit Breaker',
    description: 'Deployments mit hoher Fehlerquote eine Weile auslassen.',
    fields: [
      { path: 'circuitBreaker.enabled', label: 'Aktiv', kind: 'bool', def: 'an' },
      { path: 'circuitBreaker.failureRate', label: 'Fehlerquote (0–1]', kind: 'number', decimal: true, min: 0, max: 1, def: '0.5' },
      { path: 'circuitBreaker.minRequests', label: 'Mindestanfragen', kind: 'number', min: 1, def: '5' },
      { path: 'circuitBreaker.windowSecs', label: 'Fenster (s)', kind: 'number', min: 1, max: 3600, def: '30' },
      { path: 'circuitBreaker.openSecs', label: 'Offen für (s)', kind: 'number', min: 1, def: '30' },
    ],
  },
  {
    id: 'healthCheck',
    title: 'Health Checks',
    description: 'Aktive Prüfung je Deployment (Chat-Ping); ohne Angabe aus.',
    fields: [
      { path: 'healthCheck.enabled', label: 'Aktiv', kind: 'bool', def: 'an, sobald der Abschnitt gesetzt ist' },
      { path: 'healthCheck.intervalSecs', label: 'Abstand (s)', kind: 'number', min: 1, def: '30' },
      { path: 'healthCheck.timeoutMs', label: 'Zeitlimit (ms)', kind: 'number', min: 1, def: '5000' },
      { path: 'healthCheck.unhealthyThreshold', label: 'Fehlschläge bis „krank“', kind: 'number', min: 1, def: '2' },
    ],
  },
  { ...TIMEOUTS, description: 'Gilt für alle Deployments der Gruppe ohne eigenen Abschnitt (F44).' },
  {
    id: 'behaviour',
    title: 'Verhalten',
    description: 'Antwort, Reasoning, Betas und Freigaben (10-routing §2b).',
    fields: [
      {
        path: 'responseModel',
        label: 'Modellname in der Antwort',
        kind: 'choice',
        choices: [
          { value: 'upstream', label: 'wie vom Anbieter gemeldet' },
          { value: 'requested', label: 'angefragter Name (Gruppe/Alias)' },
        ],
        def: 'upstream',
        hint: 'Header x-aigw-model trägt immer das Anbieter-Modell (F49).',
      },
      { path: 'exposeReasoning', label: 'Reasoning ausgeben', kind: 'bool', def: 'an', hint: 'reasoning_content an OpenAI-Clients (F10).' },
      {
        path: 'clientBetas',
        label: 'anthropic-beta der Clients',
        kind: 'choice',
        choices: [
          { value: 'allow-all', label: 'alle weitergeben' },
          { value: 'allowlist', label: 'nur erlaubte' },
          { value: 'deny', label: 'verwerfen' },
        ],
        def: 'allow-all',
      },
      { path: 'betaAllowlist', label: 'Erlaubte Betas', kind: 'list', hint: 'Kennungen, mit Komma getrennt.', when: (s) => s.clientBetas === 'allowlist' },
      { path: 'fallbackIgnoresGrants', label: 'Fallbacks ohne Freigabeprüfung', kind: 'bool', def: 'aus', hint: 'Fallback-Gruppen auch ohne Freigabe für Key/Team (F80).' },
      THINKING,
      TRACE,
    ],
  },
];

export const DEPLOYMENT_SECTIONS: SettingSection[] = [
  TIMEOUTS,
  { id: 'behaviour', title: 'Verhalten', description: 'Erbt sonst von der Modellgruppe.', fields: [THINKING, TRACE] },
];

// ------------------------------------------------------------------ Pfad-Hilfen

export function getPath(o: Record<string, unknown>, path: string): unknown {
  let cur: unknown = o;
  for (const k of path.split('.')) {
    if (!cur || typeof cur !== 'object' || Array.isArray(cur)) return undefined;
    cur = (cur as Record<string, unknown>)[k];
  }
  return cur;
}

/** Setzt bzw. entfernt (`undefined`) einen Wert; leere Abschnitte entfallen. Liefert ein neues Objekt. */
export function setPath(o: Record<string, unknown>, path: string, value: unknown): Record<string, unknown> {
  const keys = path.split('.');
  const out = JSON.parse(JSON.stringify(o ?? {})) as Record<string, unknown>;
  let cur = out;
  for (const k of keys.slice(0, -1)) {
    const next = cur[k];
    if (!next || typeof next !== 'object' || Array.isArray(next)) cur[k] = {};
    cur = cur[k] as Record<string, unknown>;
  }
  const last = keys[keys.length - 1];
  if (value === undefined) delete cur[last];
  else cur[last] = value;
  // Abschnitte sind höchstens eine Ebene tief: leeren Abschnitt entfernen.
  if (keys.length > 1 && Object.keys(cur).length === 0) delete out[keys[0]];
  return out;
}

/** Bekannte Pfade (für den Hinweis „weitere Schlüssel nur in Erweitert“). */
export function unknownKeys(o: Record<string, unknown>, sections: SettingSection[]): string[] {
  const known = new Set(sections.flatMap((s) => s.fields.map((f) => f.path.split('.')[0])));
  return Object.keys(o).filter((k) => !known.has(k));
}
