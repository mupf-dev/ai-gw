// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Navigationsstruktur und Seitenkatalog der Admin-UI (AIGW-48, AIGW-49). `ready` = Seite vorhanden,
// sonst zeigt die Shell einen Platzhalter mit der Kurzbeschreibung aus diesem Katalog.
// `adminOnly` = nur für globale Admins in Navigation und Befehlspalette.
import type { IconName } from './ui/Icon.svelte';

export type PageId =
  | 'overview'
  | 'playground'
  | 'models'
  | 'providers'
  | 'prices'
  | 'keys'
  | 'admin-keys'
  | 'teams'
  | 'limits'
  | 'guardrails'
  | 'cache'
  | 'mcp'
  | 'logs'
  | 'audit'
  | 'settings'
  | 'design';

export interface Page {
  id: PageId;
  path: string;
  label: string;
  icon: IconName;
  description: string;
  ready: boolean;
  keywords?: string;
  adminOnly?: boolean;
  /** Seite mit echten Daten der Admin-API: ohne Mock-Modus nur angemeldet (sonst Anmelde-Hinweis). */
  needsAuth?: boolean;
}

export interface NavGroup {
  label?: string;
  items: Page[];
}

const p = (id: PageId, path: string, label: string, icon: IconName, description: string, ready = false, keywords = ''): Page => ({
  id,
  path,
  label,
  icon,
  description,
  ready,
  keywords,
});

export const PAGES: Record<PageId, Page> = {
  overview: p('overview', '/', 'Übersicht', 'home', 'Kennzahlen des Gateways: Requests, Tokens, Kosten, Latenz, Fehler und Cache-Treffer im Zeitverlauf.', true, 'dashboard start kpi'),
  playground: p('playground', '/playground', 'Playground', 'play', 'Modelle und Routen direkt ausprobieren: Chat, Vergleich mehrerer Modelle nebeneinander, Code-Snippets für OpenAI/Anthropic-SDKs.', false, 'chat test ausprobieren'),
  models: { ...p('models', '/models', 'Modelle & Routen', 'route', 'Modellgruppen mit Aliassen, Routing-Strategie, Fallback-Kette und Einstellungen; Deployments je Gruppe mit Priorität, Gewicht und Kapazität.', true, 'routing deployment fallback alias'), needsAuth: true },
  providers: { ...p('providers', '/providers', 'Anbieter', 'plug', 'Anbieter-Zugangsdaten (OpenAI, Azure, Anthropic, Bedrock, Vertex, Ollama, vLLM …): Werte nur schreibbar, nie lesbar; Verwendung durch Deployments.', true, 'provider upstream credentials zugangsdaten secret'), needsAuth: true },
  prices: { ...p('prices', '/prices', 'Preise', 'coins', 'Versionierte Preise je Anbieter-Modell (pro 1 Mio. Tokens, je Anfrage/Bild); gültige Versionen sind unveränderlich.', true, 'kosten pricing tokens'), needsAuth: true },
  keys: { ...p('keys', '/keys', 'Keys', 'key', 'Virtuelle API-Keys: Erzeugen (Klartext nur einmal), Rotieren, Sperren; erlaubte Modelle, Budgets und Ratenlimits je Key.', true, 'api schlüssel token virtual'), needsAuth: true },
  teams: { ...p('teams', '/teams', 'Teams & Nutzer', 'users', 'Organisationen, Teams und Nutzer mit Rollen (Admin, Org-/Team-Admin, Betrachter, Auditor) und OIDC-Zuordnung.', true, 'rollen oidc org organisation nutzer'), needsAuth: true },
  'admin-keys': { ...p('admin-keys', '/admin-keys', 'Admin-API-Keys', 'lock', 'Zugänge für Automatisierung (GitOps, CI, Monitoring) zur Admin-API mit Rollen und Ablauf; nur globale Admins.', true, 'admin token automation ci'), adminOnly: true, needsAuth: true },
  limits: p('limits', '/limits', 'Limits & Budgets', 'gauge', 'Ratenlimits (RPM/TPM, gleitendes Fenster) und Budgets (Tag/Monat) je Organisation, Team, Key und Modell; Alarmierung bei Schwellen.', false, 'rate budget quota kosten'),
  guardrails: p('guardrails', '/guardrails', 'Guardrails & Plugins', 'shield', 'Eingangs-/Ausgangsprüfungen (PII, Prompt-Injection, Moderation), Webhook- und WASM-Plugins mit Reihenfolge und Testlauf.', false, 'pii moderation wasm webhook'),
  cache: p('cache', '/cache', 'Cache', 'database', 'Exakter und semantischer Cache: Trefferquote, Ersparnis, TTL-Regeln, Vektorindex und gezieltes Leeren.', false, 'semantic vector ttl'),
  mcp: p('mcp', '/mcp', 'MCP-Server', 'network', 'Angebundene MCP-Server und Tools, Freigaben je Team/Key, Aufruf-Protokoll und Gesundheitszustand.', false, 'tools model context protocol'),
  logs: p('logs', '/logs', 'Logs', 'list', 'Request-Explorer: Filter nach Status, Modell, Key, Team und Zeitraum; Detailansicht mit Request, Response, Fallback-Versuchen und Zeitleiste.', true, 'requests traces explorer'),
  audit: p('audit', '/audit', 'Audit', 'clipboard', 'Unveränderliches Änderungsprotokoll der Steuerungsebene: wer hat wann was geändert (inkl. Diff), Export.', false, 'änderungen protokoll compliance'),
  settings: p('settings', '/settings', 'Einstellungen', 'settings', 'Anmeldung und Rollen, Darstellung, Datenquelle (Mock-Modus), GitOps-Export und -Apply (Trockenlauf mit Diff).', true, 'theme darstellung mock config gitops yaml export apply'),
  design: p('design', '/design', 'Design-System', 'sparkles', 'Komponentenübersicht des Design-Systems (Entwicklerseite).', true, 'komponenten tokens ui kit'),
};

export const NAV: NavGroup[] = [
  { items: [PAGES.overview, PAGES.playground] },
  { label: 'Gateway', items: [PAGES.models, PAGES.providers, PAGES.prices, PAGES.guardrails, PAGES.cache, PAGES.mcp] },
  { label: 'Zugriff', items: [PAGES.keys, PAGES.teams, PAGES.limits, PAGES['admin-keys']] },
  { label: 'Beobachten', items: [PAGES.logs, PAGES.audit] },
];

export const NAV_FOOTER: Page[] = [PAGES.settings];

export function pageForPath(path: string): Page | undefined {
  const clean = path.split('?')[0].replace(/\/+$/, '') || '/';
  return Object.values(PAGES).find((pg) => pg.path === clean);
}
