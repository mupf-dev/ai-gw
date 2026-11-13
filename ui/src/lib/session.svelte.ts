// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Anmeldung der Admin-UI (AIGW-32, AIGW-49): beim Start /admin/api/v1/auth/info abfragen (immer 200,
// enthält ggf. den Principal).
// - Mit OIDC: ohne Session (und ohne Mock-Modus) bzw. bei 401 → Weiterleitung zum Login; nach einem
//   abgelehnten Login (?auth_error=…) stattdessen Hinweis, damit keine Schleife entsteht.
// - Ohne OIDC (nur Bootstrap-Token/Admin-API-Keys): Token-Dialog; das Token liegt nur im sessionStorage
//   (endet mit dem Tab) und geht als Bearer an die Admin-API.
// Schreibende Anfragen mit Session tragen das CSRF-Token (siehe api/client.ts).
import { api, onUnauthorized, setBearer, setCsrfToken } from './api/client';
import type { components } from './api/schema';
import { toast } from './toast.svelte';

export type SessionInfo = components['schemas']['SessionInfo'];
type RoleBinding = components['schemas']['RoleBinding'];

const TOKEN_KEY = 'aigw.adminToken';

export const auth = $state<{
  loaded: boolean;
  oidc: boolean;
  loginUrl: string | null;
  session: SessionInfo | null;
  /** Token-Dialog offen (Bootstrap-Token/Admin-API-Key). */
  tokenDialog: boolean;
  /** Fehlermeldung im Token-Dialog. */
  tokenError: string | null;
  /** Modus „nur YAML“ erkannt (403 read-only bei einer Änderung). */
  readOnly: boolean;
}>({ loaded: false, oidc: false, loginUrl: null, session: null, tokenDialog: false, tokenError: null, readOnly: false });

const AUTH_ERRORS: Record<string, string> = {
  'no-access': 'Ihrem Konto ist keine Rolle in ai-gw zugeordnet.',
  state: 'Die Anmeldung ist abgelaufen oder wurde in einem anderen Fenster begonnen.',
  token: 'Der Identitätsanbieter hat kein gültiges Token geliefert.',
  idp: 'Der Identitätsanbieter hat die Anmeldung abgelehnt.',
  internal: 'Interner Fehler – Details im Server-Log.',
  invite: 'Einladungslink ungültig, abgelaufen oder bereits verwendet.',
};

export const ROLE_LABEL: Record<RoleBinding['role'], string> = {
  admin: 'Admin',
  'org-admin': 'Org-Admin',
  'team-admin': 'Team-Admin',
  viewer: 'Betrachter',
  auditor: 'Auditor',
  gitops: 'GitOps (CI)',
};

export const METHOD_LABEL: Record<string, string> = {
  session: 'OIDC-Session',
  'admin-api-key': 'Admin-API-Key',
  'bootstrap-token': 'Bootstrap-Token',
};

/** Eine Rollenzuweisung für die Anzeige, z. B. „Team-Admin · platform“. */
export function bindingLabel(r: RoleBinding): string {
  const scope = r.team ?? r.organization;
  return scope ? `${ROLE_LABEL[r.role]} · ${scope}` : `${ROLE_LABEL[r.role]} (global)`;
}

/** Rolle(n) für die Anzeige. */
export function roleLabel(roles: RoleBinding[]): string {
  return roles.length === 0 ? 'Keine Rolle' : roles.map(bindingLabel).join(', ');
}

/** Anzeigename und Initialen der Session. */
export function identity(s: SessionInfo): { name: string; initials: string } {
  const name = s.displayName ?? s.email ?? s.subject.replace(/^[a-z-]+:/, '');
  const parts = name.split(/[\s@._-]+/).filter(Boolean);
  const initials = ((parts[0]?.[0] ?? '?') + (parts.length > 1 ? (parts[1]?.[0] ?? '') : '')).toUpperCase();
  return { name, initials };
}

function readToken(): string | null {
  try {
    return sessionStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}
function writeToken(t: string | null) {
  try {
    if (t) sessionStorage.setItem(TOKEN_KEY, t);
    else sessionStorage.removeItem(TOKEN_KEY);
  } catch {
    /* sessionStorage nicht verfügbar – Token gilt nur bis zum Neuladen */
  }
}

export function login() {
  const back = location.pathname + location.search.replace(/[?&]auth_error=[^&]*/, '');
  location.assign(`${auth.loginUrl ?? '/admin/auth/login'}?returnTo=${encodeURIComponent(back)}`);
}

/** Anmeldung anfordern: OIDC → Login-Seite, sonst Token-Dialog. */
export function requestLogin(reason: string | null = null) {
  if (auth.oidc) {
    if (!new URLSearchParams(location.search).has('auth_error')) login();
    return;
  }
  auth.tokenError = reason;
  auth.tokenDialog = true;
}

export async function logout() {
  if (auth.session?.method === 'session') {
    const { data } = await api.POST('/admin/auth/logout');
    setCsrfToken(null);
    auth.session = null;
    location.assign(data?.redirectUrl ?? '/admin/');
    return;
  }
  // Bearer-Token: nur im Browser vergessen (das Token selbst bleibt gültig).
  writeToken(null);
  setBearer(null);
  auth.session = null;
  toast('info', 'Abgemeldet', 'Das Token wurde aus diesem Tab entfernt.');
}

async function fetchInfo(): Promise<boolean> {
  const { data } = await api.GET('/admin/api/v1/auth/info');
  if (!data) return false;
  auth.oidc = data.oidc;
  auth.loginUrl = data.loginUrl ?? null;
  auth.session = data.session ?? null;
  setCsrfToken(data.session?.csrfToken ?? null);
  return true;
}

/** Token prüfen (über auth/info) und bei Erfolg für diesen Tab merken. */
export async function submitToken(token: string): Promise<boolean> {
  const t = token.trim();
  setBearer(t);
  try {
    await fetchInfo();
  } catch {
    auth.tokenError = 'Gateway nicht erreichbar.';
    setBearer(readToken());
    return false;
  }
  if (!auth.session) {
    setBearer(readToken());
    auth.tokenError = 'Token ungültig, abgelaufen oder deaktiviert (Bootstrap-Token nach dem ersten globalen Admin).';
    return false;
  }
  writeToken(t);
  auth.tokenDialog = false;
  auth.tokenError = null;
  toast('success', 'Angemeldet', `${METHOD_LABEL[auth.session.method] ?? auth.session.method}: ${roleLabel(auth.session.roles)}`);
  return true;
}

let started = false;
/** Beim Start: Anmeldung prüfen. `wantsReal`: ohne Session echte Daten verlangt (Mock aus). */
export function loadSession(wantsReal: () => boolean) {
  if (started) return;
  started = true;
  setBearer(readToken());
  onUnauthorized(() => {
    if (auth.session?.method !== 'session') {
      // Token abgelaufen/widerrufen → vergessen und neu abfragen
      writeToken(null);
      setBearer(null);
    }
    auth.session = null;
    requestLogin('Anmeldung abgelaufen oder ungültig – bitte erneut anmelden.');
  });
  fetchInfo()
    .then((ok) => {
      if (!ok) return;
      if (!auth.session && readToken()) {
        writeToken(null);
        setBearer(null);
      }
      const error = new URLSearchParams(location.search).get('auth_error');
      if (error) toast('danger', 'Anmeldung fehlgeschlagen', AUTH_ERRORS[error] ?? error);
      else if (!auth.session && (auth.oidc || wantsReal())) requestLogin();
    })
    .catch(() => {
      /* Gateway nicht erreichbar – die UI zeigt das über die Versionsabfrage */
    })
    .finally(() => (auth.loaded = true));
}
