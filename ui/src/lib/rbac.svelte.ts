// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// Rollenprüfung in der UI: dieselben Regeln wie crates/aigw-admin/src/rbac.rs (Admin-API §5.4), nur zum
// Ausblenden von Schreibaktionen – die Entscheidung trifft immer der Server (403).
// Im Mock-Modus ohne Anmeldung gilt der Beispielnutzer als globaler Admin. Im Nur-YAML-Modus (auth/info
// `readOnly`, F56) gibt es keine Schreibaktionen – außer GitOps-Apply (eigene Prüfung in GitOpsCard).
import type { RoleBinding } from './api/resources';
import { isMock } from './mode.svelte';
import { auth } from './session.svelte';

export type Target =
  | { kind: 'global' }
  | { kind: 'catalog' }
  | { kind: 'organization'; organization: string }
  | { kind: 'team'; organization: string; team: string }
  | { kind: 'in-team'; organization: string; team: string }
  /** Objekte einer Organisation ohne Team (Keys ohne Team, F193). */
  | { kind: 'in-organization'; organization: string };

const MOCK_ROLES: RoleBinding[] = [{ role: 'admin' }];

export function roles(): RoleBinding[] {
  if (auth.session) return auth.session.roles;
  return isMock() ? MOCK_ROLES : [];
}

function effective(b: RoleBinding): boolean {
  if (b.role === 'org-admin') return !!(b.organization || b.team);
  if (b.role === 'team-admin') return !!b.team;
  return true;
}

function orgOf(t: Target): string | null {
  return t.kind === 'organization' || t.kind === 'team' || t.kind === 'in-team' || t.kind === 'in-organization' ? t.organization : null;
}

export function bindingAllows(b: RoleBinding, write: boolean, t: Target): boolean {
  if (!effective(b) || b.role === 'gitops') return false;
  const global = !b.organization && !b.team;
  if (t.kind === 'catalog') return !write || (b.role === 'admin' && global);
  let covers: boolean;
  if (global) covers = true;
  else if (!b.team) covers = orgOf(t) === b.organization;
  else if (t.kind === 'team' || t.kind === 'in-team') covers = t.team === b.team && (!b.organization || t.organization === b.organization);
  else if (t.kind === 'organization') covers = !write && b.organization === t.organization;
  else covers = false;
  if (!covers) return false;
  if (!write || b.role === 'admin') return true;
  if (b.role === 'org-admin') return t.kind === 'team' || t.kind === 'in-team' || t.kind === 'in-organization';
  if (b.role === 'team-admin') return t.kind === 'in-team';
  return false;
}

export function can(write: boolean, t: Target): boolean {
  if (write && auth.readOnly) return false;
  return roles().some((b) => bindingAllows(b, write, t));
}

export const canWrite = (t: Target) => can(true, t);
export const isGlobalAdmin = () => roles().some((b) => b.role === 'admin' && !b.organization && !b.team);
/** Darf irgendetwas schreiben (z. B. Keys irgendeines Teams)? */
export const canWriteSomething = () => !auth.readOnly && roles().some((b) => effective(b) && (b.role === 'admin' || b.role === 'org-admin' || b.role === 'team-admin'));
/** Globale Leserolle (Export, Nutzerliste, Admin-API-Keys lesen). */
export const canReadGlobal = () => roles().some((b) => !b.organization && !b.team && effective(b));

/** Globaler Admin mit Schreibrecht (nicht im Nur-YAML-Modus) – z. B. Admin-API-Keys, Nutzer, Einladungen. */
export const canAdminWrite = () => !auth.readOnly && isGlobalAdmin();

/** Schreibrecht für Keys: ohne Team in der Organisation (F193), sonst im Team. */
export function canWriteKey(team: string | null | undefined, organization: string | null | undefined): boolean {
  if (!team) return organization ? canWrite({ kind: 'in-organization', organization }) : canWrite({ kind: 'global' });
  return canWrite({ kind: 'in-team', organization: organization ?? '', team });
}
