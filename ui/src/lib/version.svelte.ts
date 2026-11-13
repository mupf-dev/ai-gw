// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Gateway-Version über die echte Admin-API (einziger Endpunkt bisher) – auch im Mock-Modus echt.
import { api, type VersionInfo } from './api/client';

export const gateway = $state<{ info: VersionInfo | null; error: string | null; loaded: boolean }>({
  info: null,
  error: null,
  loaded: false,
});

let started = false;
export function loadVersion() {
  if (started) return;
  started = true;
  api
    .GET('/admin/api/v1/version')
    .then(({ data, response }) => {
      if (data) gateway.info = data;
      else gateway.error = `HTTP ${response.status}`;
    })
    .catch((e: unknown) => {
      gateway.error = e instanceof Error ? e.message : String(e);
    })
    .finally(() => (gateway.loaded = true));
}
