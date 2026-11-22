<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts" module>
  import type { Snippet } from 'svelte';
  export interface Column<T> {
    id: string;
    label: string;
    cell: Snippet<[T]>;
    sort?: (r: T) => string | number;
    align?: 'left' | 'right';
    class?: string;
    /** Spalte erst ab dieser Breite zeigen (schmale Bildschirme). */
    from?: 'sm' | 'md' | 'lg' | 'xl';
    srOnlyLabel?: boolean;
  }

  /** Serverseitige Pagination (D11): Seite, Blättern und Seitengröße kommen von außen (Paged). */
  export interface ServerPaging {
    index: number;
    hasPrev: boolean;
    hasNext: boolean;
    pageSize: number;
    loading: boolean;
    prevPage(): void;
    nextPage(): void;
    setPageSize(n: number): void;
    /** Suchtext an den Server (entprellt). */
    search(q: string): void;
    q: string;
  }
</script>

<script lang="ts" generics="T">
  // Datentabelle: Textsuche, Filter (toolbar-Snippet), Sortierung (aria-sort), Pagination (Bits UI),
  // Leer-/Kein-Treffer-Zustand, optional klickbare Zeilen (zusätzlich ein fokussierbares Element in der Zeile).
  import { Pagination } from 'bits-ui';
  import EmptyState from './EmptyState.svelte';
  import Icon from './Icon.svelte';
  import Select from './Select.svelte';
  import { btn, cx } from './styles';

  let {
    rows,
    columns,
    key,
    caption,
    search,
    searchPlaceholder = 'Suchen …',
    toolbar,
    empty,
    onrowclick,
    selectedKey,
    sortId = $bindable(''),
    asc = $bindable(true),
    perPage = $bindable(10),
    dense = false,
    testid = 'table',
    resetFilters,
    q = $bindable(''),
    server,
    filtered = false,
  }: {
    /** Serverseitige Pagination/Suche statt Filtern im Browser. */
    server?: ServerPaging;
    /** Server-Modus: Filter aktiv (leere Seite = „keine Treffer“ statt „noch keine Einträge“). */
    filtered?: boolean;
    /** Suchtext (bindbar, z. B. aus ?q=). */
    q?: string;
    rows: T[];
    columns: Column<T>[];
    key: (r: T) => string;
    caption: string;
    search?: (r: T) => string;
    searchPlaceholder?: string;
    toolbar?: Snippet;
    empty?: Snippet;
    onrowclick?: (r: T) => void;
    selectedKey?: string | null;
    sortId?: string;
    asc?: boolean;
    perPage?: number;
    dense?: boolean;
    testid?: string;
    /** Wird bei „Filter zurücksetzen“ zusätzlich zur Suche aufgerufen. */
    resetFilters?: () => void;
  } = $props();

  let page = $state(1);
  const FROM = { sm: 'hidden sm:table-cell', md: 'hidden md:table-cell', lg: 'hidden lg:table-cell', xl: 'hidden xl:table-cell' };

  const view = $derived.by(() => {
    if (server) return rows;
    const needle = q.trim().toLowerCase();
    const r = needle && search ? rows.filter((x) => search(x).toLowerCase().includes(needle)) : [...rows];
    const col = columns.find((c) => c.id === sortId);
    if (col?.sort) {
      const f = col.sort;
      r.sort((a, b) => {
        const x = f(a);
        const y = f(b);
        const c = typeof x === 'number' && typeof y === 'number' ? x - y : String(x).localeCompare(String(y), 'de', { numeric: true });
        return asc ? c : -c;
      });
    }
    return r;
  });
  const pages = $derived(Math.max(1, Math.ceil(view.length / perPage)));
  $effect(() => {
    if (page > pages) page = pages;
  });
  const visible = $derived(server ? rows : view.slice((page - 1) * perPage, page * perPage));
  const PAGE_SIZES = [10, 25, 50, 100];

  function sortBy(id: string) {
    if (sortId === id) asc = !asc;
    else {
      sortId = id;
      asc = true;
    }
  }
  function rowClick(e: MouseEvent, r: T) {
    if (!onrowclick) return;
    if ((e.target as Element).closest('a, button, input, select, label, [role=menuitem]')) return;
    if (window.getSelection()?.toString()) return;
    onrowclick(r);
  }
  function reset() {
    q = '';
    server?.search('');
    resetFilters?.();
  }
  const cellPad = $derived(dense ? 'px-3 py-1.5' : 'px-3 py-2.5');
</script>

<div class="flex min-w-0 flex-col" data-testid={testid}>
  <div class="flex flex-wrap items-center gap-2 border-b border-line px-3 py-2.5">
    {#if search}
      <label class="relative flex min-w-48 flex-1 items-center sm:max-w-72">
        <span class="sr-only">{searchPlaceholder}</span>
        <span class="pointer-events-none absolute left-2.5 text-fg-subtle"><Icon name="search" size={14} /></span>
        <input
          type="search"
          value={server ? server.q : q}
          oninput={(e) => {
            const v = e.currentTarget.value;
            if (server) server.search(v);
            else {
              q = v;
              page = 1;
            }
          }}
          placeholder={searchPlaceholder}
          class="h-8 w-full rounded-md border border-line bg-surface pr-2 pl-8 text-sm text-fg placeholder:text-fg-subtle hover:border-line-strong focus-visible:border-accent focus-visible:outline-0"
        />
      </label>
    {/if}
    {@render toolbar?.()}
    {#if server}
      <span class="ml-auto text-xs text-fg-muted num" aria-live="polite">{server.loading ? 'lädt …' : `Seite ${server.index + 1} · ${rows.length} Einträge${server.hasNext ? ' (weitere folgen)' : ''}`}</span>
    {:else}
      <span class="ml-auto text-xs text-fg-muted num" aria-live="polite">{view.length === rows.length ? `${rows.length} Einträge` : `${view.length} von ${rows.length}`}</span>
    {/if}
  </div>

  <div class="scrollbar-thin overflow-x-auto">
    <table class="w-full border-collapse text-left text-base">
      <caption class="sr-only">{caption}</caption>
      <thead>
        <tr class="border-b border-line bg-surface-2">
          {#each columns as c (c.id)}
            <th
              scope="col"
              class={[cellPad, 'h-9 py-0 font-medium whitespace-nowrap text-fg-muted', c.align === 'right' && 'text-right', c.from && FROM[c.from], c.class]}
              aria-sort={c.sort && sortId === c.id ? (asc ? 'ascending' : 'descending') : undefined}
            >
              {#if c.sort && !server}
                <button
                  type="button"
                  class={['inline-flex items-center gap-1 rounded-sm text-sm hover:text-fg', c.align === 'right' && 'flex-row-reverse', sortId === c.id && 'text-fg']}
                  onclick={() => sortBy(c.id)}
                >
                  {c.label}
                  <Icon name={sortId === c.id ? (asc ? 'arrow-up' : 'arrow-down') : 'arrow-up-down'} size={12} class={sortId === c.id ? '' : 'opacity-50'} />
                </button>
              {:else if c.srOnlyLabel}
                <span class="sr-only">{c.label}</span>
              {:else}
                <span class="text-sm">{c.label}</span>
              {/if}
            </th>
          {/each}
        </tr>
      </thead>
      <tbody>
        {#if visible.length === 0}
          <tr>
            <td colspan={columns.length}>
              {#if server ? filtered || !!server.q || server.index > 0 : rows.length > 0}
                <EmptyState compact icon="search" title="Keine Treffer" text="Kein Eintrag passt zu Suche und Filtern.">
                  <button type="button" class={btn('secondary', 'sm')} onclick={reset}>Filter zurücksetzen</button>
                </EmptyState>
              {:else}
                {@render empty?.()}
              {/if}
            </td>
          </tr>
        {:else}
          {#each visible as r (key(r))}
            <!-- Zeilenklick ist Zeigerkomfort; per Tastatur führt das Element in der ersten Zelle zum selben Ziel. -->
            <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
            <tr
              class={[
                'border-b border-line last:border-b-0 transition-colors',
                onrowclick && 'cursor-pointer hover:bg-surface-hover',
                selectedKey === key(r) && 'bg-accent-soft',
              ]}
              data-key={key(r)}
              onclick={(e) => rowClick(e, r)}
            >
              {#each columns as c (c.id)}
                <td class={[cellPad, 'align-middle', c.align === 'right' && 'text-right', c.from && FROM[c.from], c.class]}>{@render c.cell(r)}</td>
              {/each}
            </tr>
          {/each}
        {/if}
      </tbody>
    </table>
  </div>

  {#if server && (rows.length > 0 || server.index > 0)}
    <div class="flex flex-wrap items-center gap-3 border-t border-line px-3 py-2 text-sm text-fg-muted" data-testid="{testid}-pager">
      <span class="num">Seite {server.index + 1}</span>
      <div class="hidden items-center gap-2 sm:flex">
        <span>Zeilen</span>
        <div class="w-20">
          <Select size="sm" label="Zeilen pro Seite" value={String(server.pageSize)} items={PAGE_SIZES.map((n) => ({ value: String(n), label: String(n) }))} onchange={(v) => server.setPageSize(Number(v))} />
        </div>
      </div>
      <div class="ml-auto flex items-center gap-1">
        <button type="button" class={btn('ghost', 'sm')} disabled={!server.hasPrev || server.loading} onclick={() => server.prevPage()} data-testid="page-prev"><Icon name="chevron-left" size={14} /> Zurück</button>
        <button type="button" class={btn('ghost', 'sm')} disabled={!server.hasNext || server.loading} onclick={() => server.nextPage()} data-testid="page-next">Weiter <Icon name="chevron-right" size={14} /></button>
      </div>
    </div>
  {:else if !server && view.length > 0}
    <div class="flex flex-wrap items-center gap-3 border-t border-line px-3 py-2 text-sm text-fg-muted">
      <span class="num">{(page - 1) * perPage + 1}–{Math.min(page * perPage, view.length)} von {view.length}</span>
      <div class="hidden items-center gap-2 sm:flex">
        <span>Zeilen</span>
        <div class="w-20">
          <Select
            size="sm"
            label="Zeilen pro Seite"
            value={String(perPage)}
            items={[10, 25, 50].map((n) => ({ value: String(n), label: String(n) }))}
            onchange={(v) => {
              perPage = Number(v);
              page = 1;
            }}
          />
        </div>
      </div>
      <Pagination.Root count={view.length} {perPage} bind:page class="ml-auto">
        {#snippet children({ pages: items })}
          <div class="flex items-center gap-1">
            <Pagination.PrevButton class={btn('ghost', 'icon-sm')} aria-label="Vorherige Seite"><Icon name="chevron-left" size={14} /></Pagination.PrevButton>
            {#each items as item (item.key)}
              {#if item.type === 'ellipsis'}
                <span class="px-1 text-fg-subtle">…</span>
              {:else}
                <Pagination.Page
                  page={item}
                  class={cx(
                    'num inline-flex h-7 min-w-7 items-center justify-center rounded-md px-1.5 text-sm text-fg-muted hover:bg-surface-hover hover:text-fg',
                    'data-selected:bg-accent-soft data-selected:font-semibold data-selected:text-accent-soft-fg',
                  )}
                >
                  {item.value}
                </Pagination.Page>
              {/if}
            {/each}
            <Pagination.NextButton class={btn('ghost', 'icon-sm')} aria-label="Nächste Seite"><Icon name="chevron-right" size={14} /></Pagination.NextButton>
          </div>
        {/snippet}
      </Pagination.Root>
    </div>
  {/if}
</div>
