// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Gateway-Version und Snapshot-Zustand (F15) über die echte Admin-API – auch im Mock-Modus echt.
// Wird alle 30 s erneut abgefragt, damit das Banner „Konfiguration veraltet“ von selbst erscheint/verschwindet.
import { api, type VersionInfo } from './api/client';

export const gateway = $state<{ info: VersionInfo | null; error: string | null; loaded: boolean }>({
  info: null,
  error: null,
  loaded: false,
});

const POLL_MS = 30_000;

async function fetchVersion() {
  try {
    const { data, response } = await api.GET('/admin/api/v1/version');
    if (data) {
      gateway.info = data;
      gateway.error = null;
    } else gateway.error = `HTTP ${response.status}`;
  } catch (e) {
    gateway.error = e instanceof Error ? e.message : String(e);
  } finally {
    gateway.loaded = true;
  }
}

let started = false;
export function loadVersion() {
  if (started) return;
  started = true;
  void fetchVersion();
  setInterval(() => {
    if (document.visibilityState === 'visible') void fetchVersion();
  }, POLL_MS);
}
