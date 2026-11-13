// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Arbeitskontext der Topbar: Organisation und Team ('' = alle Teams). Seiten filtern danach.
import { ORGS } from './mock/data';

export const ctx = $state({ orgId: ORGS[0].id, teamId: '' });

export const currentOrg = () => ORGS.find((o) => o.id === ctx.orgId) ?? ORGS[0];

export const USER = { name: 'Anna Becker', email: 'a.becker@acme.example', role: 'Admin', initials: 'AB' };
