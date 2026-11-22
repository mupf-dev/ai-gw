// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// Toasts: kurze, nicht modale Rückmeldungen über eine aria-live-Region (Toaster.svelte).
// Auto-Ausblenden pausiert, solange Zeiger oder Fokus im Toaster sind (WCAG 2.2.1).

export type ToastKind = 'success' | 'info' | 'warning' | 'danger';
export interface Toast {
  id: number;
  kind: ToastKind;
  title: string;
  text?: string;
}

export const toasts = $state<Toast[]>([]);
const timers = new Map<number, ReturnType<typeof setTimeout>>();
let seq = 0;
let paused = false;
const TTL = 5000;

function arm(id: number) {
  if (!paused) timers.set(id, setTimeout(() => dismiss(id), TTL));
}

export function toast(kind: ToastKind, title: string, text?: string) {
  const id = ++seq;
  toasts.push({ id, kind, title, text });
  if (toasts.length > 4) dismiss(toasts[0].id);
  arm(id);
}

export function dismiss(id: number) {
  clearTimeout(timers.get(id));
  timers.delete(id);
  const i = toasts.findIndex((t) => t.id === id);
  if (i >= 0) toasts.splice(i, 1);
}

export function pauseToasts(p: boolean) {
  paused = p;
  if (p) {
    for (const t of timers.values()) clearTimeout(t);
    timers.clear();
  } else for (const t of toasts) arm(t.id);
}
