// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Typisierter Client der Admin-API. Die Typen in schema.d.ts werden aus der OpenAPI-
// Spezifikation erzeugt (scripts/gen-api.sh → ui/openapi.json → pnpm gen:api).
// Schreibende Anfragen tragen das CSRF-Token der Session (Header X-CSRF-Token, AIGW-32).
import createClient from 'openapi-fetch';
import type { components, paths } from './schema';

export const api = createClient<paths>({ baseUrl: '' });

export type VersionInfo = components['schemas']['VersionInfo'];

let csrfToken: string | null = null;

/** Setzt das CSRF-Token der aktuellen Session (aus /admin/api/v1/auth/info). */
export function setCsrfToken(token: string | null) {
  csrfToken = token;
}

const SAFE = new Set(['GET', 'HEAD', 'OPTIONS']);
api.use({
  onRequest({ request }) {
    if (csrfToken && !SAFE.has(request.method)) request.headers.set('X-CSRF-Token', csrfToken);
    return request;
  },
});
