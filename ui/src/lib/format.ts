// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Zahlen-, Geld- und Zeitformate (de-DE). Große Zahlen kompakt (1,2 Mio.), Geld in der Abrechnungswährung
// (`billing.currency` aus auth/info, AIGW-117; Standard USD).

const LOCALE = 'de-DE';
const nf = new Intl.NumberFormat(LOCALE);
const compact = new Intl.NumberFormat(LOCALE, { notation: 'compact', maximumFractionDigits: 1 });
let cur = new Intl.NumberFormat(LOCALE, { style: 'currency', currency: 'USD' });
let curSmall = new Intl.NumberFormat(LOCALE, { style: 'currency', currency: 'USD', maximumFractionDigits: 4 });

/** Abrechnungswährung setzen (ISO 4217); ungültige Codes lassen die bisherige stehen. */
export function setCurrency(code: string) {
  try {
    cur = new Intl.NumberFormat(LOCALE, { style: 'currency', currency: code });
    curSmall = new Intl.NumberFormat(LOCALE, { style: 'currency', currency: code, maximumFractionDigits: 4 });
  } catch {
    /* unbekannter Code: bisherige Formate behalten */
  }
}
const pct = new Intl.NumberFormat(LOCALE, { style: 'percent', maximumFractionDigits: 1 });
const dt = new Intl.DateTimeFormat(LOCALE, { dateStyle: 'medium', timeStyle: 'medium' });
const time = new Intl.DateTimeFormat(LOCALE, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
const date = new Intl.DateTimeFormat(LOCALE, { dateStyle: 'medium' });
const rtf = new Intl.RelativeTimeFormat(LOCALE, { numeric: 'auto' });

export const num = (n: number) => nf.format(n);
export const short = (n: number) => compact.format(n);
export const money = (n: number) => (Math.abs(n) < 0.01 && n !== 0 ? curSmall.format(n) : cur.format(n));
export const percent = (n: number) => pct.format(n);
export const dateTime = (t: number) => dt.format(t);
export const clockTime = (t: number) => time.format(t);
export const day = (t: number) => date.format(t);
export const ms = (n: number) => (n >= 1000 ? `${(n / 1000).toLocaleString(LOCALE, { maximumFractionDigits: 2 })} s` : `${Math.round(n)} ms`);

export function ago(t: number, now = Date.now()): string {
  const s = Math.round((t - now) / 1000);
  const abs = Math.abs(s);
  if (abs < 60) return rtf.format(s, 'second');
  if (abs < 3600) return rtf.format(Math.round(s / 60), 'minute');
  if (abs < 86400) return rtf.format(Math.round(s / 3600), 'hour');
  return rtf.format(Math.round(s / 86400), 'day');
}
