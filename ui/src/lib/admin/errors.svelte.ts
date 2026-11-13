// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Einheitliche Fehlerbehandlung der Verwaltungsseiten (RFC 9457 → Toast bzw. Formularfehler):
// - 412 precondition-failed → Konflikt-Dialog „neu laden“ (optimistische Sperre, ETag/If-Match)
// - 403 read-only → Hinweis auf den Modus „nur YAML“ (GitOps)
// - 401 → Anmeldung (Login/Token-Dialog, ausgelöst im API-Client)
// - 400/409/422 in Formularen → Fehlermeldung im Formular, sonst Toast
import { ApiError } from '../api/client';
import { auth } from '../session.svelte';
import { toast } from '../toast.svelte';

export const conflict = $state<{ open: boolean; what: string; reload: (() => void) | null }>({ open: false, what: '', reload: null });

export interface HandleOptions {
  /** Was wurde versucht (für Titel), z. B. „Team speichern“. */
  action: string;
  /** Formularfehler setzen statt Toast (Validierung, Konflikte). */
  form?: (message: string) => void;
  /** Bei 412: neu laden (Konflikt-Dialog). */
  reload?: () => void;
  /**
   * Feldgenaue Anzeige (AIGW-52): bekommt Feld und Meldung aus `problemField`; `true` = am Feld angezeigt,
   * sonst landet der Fehler im Formular.
   */
  field?: (field: string, message: string) => boolean;
}

/**
 * Feld eines Validierungsfehlers. Die Admin-API stellt das betroffene Feld der Meldung voran
 * („scope.target: unbekanntes Team …“, „url: …“, „settings.headers.x: …“); ein führendes `spec.`
 * wird entfernt. Ohne erkennbares Feld `null`.
 */
export function problemField(e: unknown): { field: string; message: string } | null {
  if (!(e instanceof ApiError) || !e.detail || ![400, 409, 422].includes(e.status)) return null;
  const m = /^([A-Za-z][A-Za-z0-9_-]*(?:\.[A-Za-z0-9_-]+|\[\d+\])*): (.+)$/s.exec(e.detail);
  if (!m) return null;
  return { field: m[1].replace(/^spec\./, ''), message: m[2] };
}

export function problemText(e: unknown): string {
  if (e instanceof ApiError) return e.detail ? `${e.title}: ${e.detail}` : e.title;
  return e instanceof Error ? e.message : String(e);
}

export function handleError(e: unknown, o: HandleOptions) {
  if (!(e instanceof ApiError)) {
    toast('danger', `${o.action} fehlgeschlagen`, problemText(e));
    return;
  }
  if (e.status === 412) {
    conflict.what = o.action;
    conflict.reload = o.reload ?? null;
    conflict.open = true;
    return;
  }
  if (e.status === 401) {
    toast('warning', 'Anmeldung erforderlich', 'Bitte anmelden und erneut versuchen.');
    return;
  }
  if (e.status === 403 && e.code === 'read-only') {
    auth.readOnly = true;
    toast('warning', 'Nur-YAML-Modus', 'Änderungen sind nur über GitOps möglich (Einstellungen → GitOps).');
    return;
  }
  const pf = o.field ? problemField(e) : null;
  if (pf && o.field?.(pf.field, pf.message)) {
    o.form?.('Bitte die markierten Felder prüfen.');
    return;
  }
  if (o.form && [400, 403, 404, 409, 415, 422].includes(e.status)) {
    o.form(e.detail ?? e.title);
    return;
  }
  toast('danger', `${o.action} fehlgeschlagen`, e.detail ?? e.title);
}

/** Für Bestätigungsdialoge: Formularfehler als Text (Dialog bleibt offen) oder null (Toast/Konflikt-Dialog, schließt). */
export function formErrorOf(e: unknown, action: string, reload?: () => void): string | null {
  const box: { msg: string | null } = { msg: null };
  handleError(e, { action, form: (m) => (box.msg = m), reload });
  return box.msg;
}

/** Lädt Daten mit Lade-/Fehlerzustand; `reload()` lädt neu. */
export class Loader<T> {
  data = $state<T | null>(null);
  error = $state<unknown>(null);
  loading = $state(true);
  #fn: () => Promise<T>;
  #seq = 0;
  constructor(fn: () => Promise<T>) {
    this.#fn = fn;
    void this.reload();
  }
  async reload(): Promise<void> {
    const seq = ++this.#seq;
    this.loading = true;
    try {
      const d = await this.#fn();
      if (seq !== this.#seq) return;
      this.data = d;
      this.error = null;
    } catch (e) {
      if (seq !== this.#seq) return;
      this.error = e;
    } finally {
      if (seq === this.#seq) this.loading = false;
    }
  }
}

/** Liste → Datei-Download (Blob-URL; CSP-neutral). */
export function download(name: string, text: string, type = 'application/yaml') {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** „a, b\nc“ → ['a','b','c'] (ohne Leereinträge/Dubletten). */
export function splitList(s: string): string[] {
  return [...new Set(s.split(/[\s,]+/).map((x) => x.trim()).filter(Boolean))];
}

/** Datum für <input type="datetime-local"> (lokale Zeit) ↔ RFC 3339. */
export function toLocalInput(iso: string | null | undefined): string {
  if (!iso) return '';
  const d = new Date(iso);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
export function fromLocalInput(v: string): string | null {
  if (!v) return null;
  const d = new Date(v);
  return Number.isNaN(d.getTime()) ? null : d.toISOString().replace(/\.\d{3}Z$/, 'Z');
}
