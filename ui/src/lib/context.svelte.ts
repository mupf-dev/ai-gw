// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Arbeitskontext der Topbar: Organisation und Team (Namen; '' = alle). Seiten filtern danach.
// Organisationen und Teams kommen aus der Admin-API (im Mock-Modus aus dem Mock-Backend) und werden bei
// jedem Wechsel der Datenquelle neu geladen; eingeschränkte Rollen sehen nur ihren Bereich.
import { organizations, teams, type Organization, type Team } from './api/resources';

// Vorbelegung per URL (`?org=…`, Deep-Links und Tests); der Umschalter ändert danach nur den Zustand.
const initial = new URLSearchParams(location.search);
export const ctx = $state({ orgId: initial.get('org') ?? '', teamId: '' });

export const catalog = $state<{ orgs: Organization[]; teams: Team[]; loaded: boolean }>({ orgs: [], teams: [], loaded: false });

let seq = 0;
export async function loadCatalog() {
  const mine = ++seq;
  try {
    const [o, t] = await Promise.all([organizations.list(), teams.list()]);
    if (mine !== seq) return;
    catalog.orgs = o;
    catalog.teams = t;
  } catch {
    if (mine !== seq) return;
    catalog.orgs = [];
    catalog.teams = [];
  }
  catalog.loaded = true;
  if (!catalog.orgs.some((x) => x.metadata.name === ctx.orgId)) ctx.orgId = catalog.orgs[0]?.metadata.name ?? '';
  if (ctx.teamId && !catalog.teams.some((x) => x.metadata.name === ctx.teamId)) ctx.teamId = '';
}

export const label = (o: Organization | Team | undefined | null) => o?.spec.displayName || o?.metadata.name || '';

export interface OrgView {
  id: string;
  name: string;
  teams: { id: string; name: string }[];
}

/** Aktuelle Organisation mit ihren Teams (ohne Organisationen: alle Teams). */
export function currentOrg(): OrgView {
  const o = catalog.orgs.find((x) => x.metadata.name === ctx.orgId);
  const ts = catalog.teams.filter((t) => !o || t.spec.organization === o.metadata.name);
  return { id: ctx.orgId, name: o ? label(o) : 'Alle Organisationen', teams: ts.map((t) => ({ id: t.metadata.name, name: label(t) })) };
}

export const teamLabel = (name: string | null | undefined) => (name ? label(catalog.teams.find((t) => t.metadata.name === name)) || name : '–');
export const orgOfTeam = (name: string | null | undefined) => (name ? (catalog.teams.find((t) => t.metadata.name === name)?.spec.organization ?? null) : null);

export const USER = { name: 'Anna Becker', email: 'a.becker@acme.example', role: 'Admin', initials: 'AB' };
