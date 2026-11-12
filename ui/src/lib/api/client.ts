// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Typisierter Client der Admin-API. Die Typen in schema.d.ts werden aus der OpenAPI-
// Spezifikation erzeugt (scripts/gen-api.sh → ui/openapi.json → pnpm gen:api).
import createClient from 'openapi-fetch';
import type { components, paths } from './schema';

export const api = createClient<paths>({ baseUrl: '' });

export type VersionInfo = components['schemas']['VersionInfo'];
