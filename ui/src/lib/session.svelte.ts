// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Anmeldung der Admin-UI (AIGW-32): beim Start /admin/api/v1/auth/info abfragen (immer 200,
// enthält ggf. die Session). Ohne Session und mit OIDC → Weiterleitung zum Login; nach einem
// abgelehnten Login (?auth_error=…) stattdessen Hinweis, damit keine Schleife entsteht.
// Schreibende Anfragen tragen das CSRF-Token der Session (siehe api/client.ts).
import { api, setCsrfToken } from './api/client';
import type { components } from './api/schema';
import { toast } from './toast.svelte';

export type SessionInfo = components['schemas']['SessionInfo'];
type RoleBinding = components['schemas']['RoleBinding'];

export const auth = $state<{
  loaded: boolean;
  oidc: boolean;
  loginUrl: string | null;
  session: SessionInfo | null;
}>({ loaded: false, oidc: false, loginUrl: null, session: null });

const AUTH_ERRORS: Record<string, string> = {
  'no-access': 'Ihrem Konto ist keine Rolle in ai-gw zugeordnet.',
  state: 'Die Anmeldung ist abgelaufen oder wurde in einem anderen Fenster begonnen.',
  token: 'Der Identitätsanbieter hat kein gültiges Token geliefert.',
  idp: 'Der Identitätsanbieter hat die Anmeldung abgelehnt.',
  internal: 'Interner Fehler – Details im Server-Log.',
};

const ROLE_LABEL: Record<RoleBinding['role'], string> = {
  admin: 'Admin',
  'org-admin': 'Org-Admin',
  'team-admin': 'Team-Admin',
  viewer: 'Betrachter',
  auditor: 'Auditor',
};

/** Rolle(n) für die Anzeige, z. B. „Team-Admin · platform“. */
export function roleLabel(roles: RoleBinding[]): string {
  if (roles.length === 0) return 'Keine Rolle';
  return roles
    .map((r) => {
      const scope = r.team ?? r.organization;
      return scope ? `${ROLE_LABEL[r.role]} · ${scope}` : ROLE_LABEL[r.role];
    })
    .join(', ');
}

/** Anzeigename und Initialen der Session. */
export function identity(s: SessionInfo): { name: string; initials: string } {
  const name = s.displayName ?? s.email ?? s.subject.replace(/^[a-z-]+:/, '');
  const parts = name.split(/[\s@._-]+/).filter(Boolean);
  const initials = ((parts[0]?.[0] ?? '?') + (parts.length > 1 ? (parts[1]?.[0] ?? '') : '')).toUpperCase();
  return { name, initials };
}

export function login() {
  const back = location.pathname + location.search.replace(/[?&]auth_error=[^&]*/, '');
  location.assign(`${auth.loginUrl ?? '/admin/auth/login'}?returnTo=${encodeURIComponent(back)}`);
}

export async function logout() {
  const { data } = await api.POST('/admin/auth/logout');
  setCsrfToken(null);
  auth.session = null;
  location.assign(data?.redirectUrl ?? '/admin/');
}

let started = false;
export function loadSession() {
  if (started) return;
  started = true;
  api
    .GET('/admin/api/v1/auth/info')
    .then(({ data }) => {
      if (!data) return;
      auth.oidc = data.oidc;
      auth.loginUrl = data.loginUrl ?? null;
      auth.session = data.session ?? null;
      setCsrfToken(data.session?.csrfToken ?? null);
      const error = new URLSearchParams(location.search).get('auth_error');
      if (error) {
        toast('danger', 'Anmeldung fehlgeschlagen', AUTH_ERRORS[error] ?? error);
      } else if (!auth.session && auth.oidc) {
        login();
      }
    })
    .catch(() => {
      /* Gateway nicht erreichbar – die UI zeigt das über die Versionsabfrage */
    })
    .finally(() => (auth.loaded = true));
}
