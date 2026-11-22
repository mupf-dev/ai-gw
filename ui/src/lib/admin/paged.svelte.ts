// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// Serverseitige Pagination (D11/F139): eine Seite je Abfrage (Keyset-Cursor der Admin-API), Filter und Suche
// gehen als Query an den Server. Vor-/Zurück-Blättern über einen Cursor-Stapel (der Server kennt nur „weiter“).
// Gleiche Oberfläche wie `Loader` (data/error/loading/reload), damit AsyncState beides anzeigt.
import type { Page, Query } from '../api/client';

export type FetchPage<T> = (query: Query, limit: number, cursor?: string) => Promise<Page<T>>;

export const PAGE_SIZES = [10, 25, 50, 100];

export class Paged<T> {
  data = $state<T[] | null>(null);
  error = $state<unknown>(null);
  loading = $state(true);
  /** Seitengröße (Standard 25). */
  pageSize = $state(25);
  /** Index der aktuellen Seite (0 = erste). */
  index = $state(0);
  /** Cursor der nächsten Seite (null = letzte Seite). */
  next = $state<string | null>(null);
  /** Suchtext (`?q=`), entprellt an den Server. */
  q = $state('');

  #fetch: FetchPage<T>;
  #query: () => Query;
  #cursors: (string | undefined)[] = [undefined];
  #seq = 0;
  #timer: ReturnType<typeof setTimeout> | undefined;

  constructor(fetch: FetchPage<T>, query: () => Query = () => ({}), opts: { pageSize?: number; q?: string } = {}) {
    this.#fetch = fetch;
    this.#query = query;
    if (opts.pageSize) this.pageSize = opts.pageSize;
    if (opts.q) this.q = opts.q;
    void this.reload();
  }

  get hasPrev(): boolean {
    return this.index > 0;
  }
  get hasNext(): boolean {
    return this.next !== null;
  }

  /** Aktuelle Seite neu laden (z. B. nach einer Änderung). */
  async reload(): Promise<void> {
    const seq = ++this.#seq;
    this.loading = true;
    try {
      const q = { ...this.#query(), q: this.q.trim() || undefined };
      const page = await this.#fetch(q, this.pageSize, this.#cursors[this.index]);
      if (seq !== this.#seq) return;
      // Leere Folgeseite (z. B. nach Löschen des letzten Eintrags) → eine Seite zurück.
      if (page.items.length === 0 && this.index > 0) {
        this.index -= 1;
        this.#cursors.length = this.index + 1;
        void this.reload();
        return;
      }
      this.data = page.items;
      this.next = page.nextCursor;
      this.error = null;
    } catch (e) {
      if (seq !== this.#seq) return;
      this.error = e;
    } finally {
      if (seq === this.#seq) this.loading = false;
    }
  }

  /** Zur ersten Seite (nach Filteränderung). */
  first(): Promise<void> {
    this.index = 0;
    this.#cursors = [undefined];
    return this.reload();
  }
  nextPage() {
    if (!this.next) return;
    this.#cursors[this.index + 1] = this.next;
    this.index += 1;
    void this.reload();
  }
  prevPage() {
    if (this.index === 0) return;
    this.index -= 1;
    void this.reload();
  }
  setPageSize(n: number) {
    this.pageSize = n;
    void this.first();
  }
  /** Suchtext setzen; lädt nach kurzer Pause die erste Seite. */
  search(v: string) {
    this.q = v;
    clearTimeout(this.#timer);
    this.#timer = setTimeout(() => void this.first(), 250);
  }
}
