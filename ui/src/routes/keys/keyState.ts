// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Zustände eines API-Keys (F37) für Liste und Detailseite: active | suspended (umkehrbar) | revoked (endgültig),
// dazu abgeleitet „abgelaufen“ und „läuft bald ab“ (expiresAt).
import type { ApiKey } from '../../lib/api/resources';

const DAY = 86_400_000;
export const expired = (k: ApiKey) => !!k.spec.expiresAt && Date.parse(k.spec.expiresAt) <= Date.now();
export const expiringSoon = (k: ApiKey) => k.status.state === 'active' && !!k.spec.expiresAt && !expired(k) && Date.parse(k.spec.expiresAt) - Date.now() < 14 * DAY;

export type KeyAction = 'suspend' | 'resume' | 'revoke' | 'delete';

export function keyStatus(k: ApiKey): { tone: 'success' | 'warning' | 'danger' | 'neutral'; label: string } {
  if (k.status.state === 'revoked') return { tone: 'danger', label: 'Widerrufen' };
  if (k.status.state === 'suspended') return { tone: 'warning', label: 'Gesperrt' };
  if (expired(k)) return { tone: 'neutral', label: 'Abgelaufen' };
  if (expiringSoon(k)) return { tone: 'warning', label: `Läuft ab ${new Date(k.spec.expiresAt ?? '').toLocaleDateString('de-DE')}` };
  return { tone: 'success', label: 'Aktiv' };
}

/** Statusfilter der Liste: serverseitig über `state`. */
export const STATE_FILTER: { value: string; label: string }[] = [
  { value: '', label: 'Alle Status' },
  { value: 'active', label: 'Aktiv' },
  { value: 'suspended', label: 'Gesperrt' },
  { value: 'revoked', label: 'Widerrufen' },
];
