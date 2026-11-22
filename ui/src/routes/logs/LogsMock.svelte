<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Logs-Explorer: Filterleiste, Kennzahlen der Auswahl, Tabelle (dicht), Detail-Sheet mit Request/Response.
  import { ctx } from '../../lib/context.svelte';
  import { logs } from '../../lib/data.svelte';
  import { ago, clockTime, money, ms, num, percent } from '../../lib/format';
  import { MODEL_GROUPS, PROVIDERS } from '../../lib/mock/data';
  import type { LogEntry } from '../../lib/mock/types';
  import { route } from '../../lib/router.svelte';
  import Badge from '../../lib/ui/Badge.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../../lib/ui/DataTable.svelte';
  import EmptyState from '../../lib/ui/EmptyState.svelte';
  import Icon from '../../lib/ui/Icon.svelte';
  import PageHeader from '../../lib/ui/PageHeader.svelte';
  import Select from '../../lib/ui/Select.svelte';
  import Tooltip from '../../lib/ui/Tooltip.svelte';
  import LogDetail from './LogDetail.svelte';

  const params = new URLSearchParams(route.search);
  let status = $state(params.get('status') ?? 'all');
  let model = $state('all');
  let cache = $state('all');
  let q = $state('');
  let selected = $state<LogEntry | null>(null);
  let detailOpen = $state(false);
  let live = $state(true);

  const res = $derived(logs());
  const all = $derived(res.state === 'ok' ? res.data : []);
  const rows = $derived(
    all.filter(
      (e) =>
        (status === 'all' ||
          (status === 'ok' && e.status < 300) ||
          (status === 'error' && e.status >= 400) ||
          (status === '429' && e.status === 429) ||
          (status === '5xx' && e.status >= 500)) &&
        (model === 'all' || e.model === model) &&
        (cache === 'all' || (cache === 'hit' ? e.cache !== 'miss' : e.cache === 'miss')) &&
        (!ctx.teamId || e.teamId === ctx.teamId),
    ),
  );
  const stats = $derived.by(() => {
    const n = rows.length || 1;
    const lat = rows.map((e) => e.latency).sort((a, b) => a - b);
    return {
      count: rows.length,
      errors: rows.filter((e) => e.status >= 400).length / n,
      p95: lat[Math.floor(lat.length * 0.95)] ?? 0,
      cost: rows.reduce((a, e) => a + e.cost, 0),
    };
  });

  function open(e: LogEntry) {
    selected = e;
    detailOpen = true;
  }
  const statusTone = (s: number) => (s < 300 ? 'success' : s === 429 ? 'warning' : 'danger') as 'success' | 'warning' | 'danger';

  const columns: Column<LogEntry>[] = [
    { id: 'ts', label: 'Zeit', cell: timeCell, sort: (e) => e.ts, class: 'w-28' },
    { id: 'status', label: 'Status', cell: statusCell, sort: (e) => e.status, class: 'w-20' },
    { id: 'model', label: 'Modell', cell: modelCell, sort: (e) => e.model },
    { id: 'key', label: 'Key', cell: keyCell, sort: (e) => e.keyName, from: 'md' },
    { id: 'tokens', label: 'Tokens', cell: tokCell, sort: (e) => e.inTok + e.outTok, align: 'right', from: 'lg' },
    { id: 'cost', label: 'Kosten', cell: costCell, sort: (e) => e.cost, align: 'right', from: 'lg' },
    { id: 'latency', label: 'Latenz', cell: latCell, sort: (e) => e.latency, align: 'right' },
    { id: 'cache', label: 'Cache', cell: cacheCell, from: 'xl', class: 'w-24' },
  ];
</script>

{#snippet timeCell(e: LogEntry)}
  <button type="button" class="num text-left text-sm text-fg hover:text-accent-text" onclick={() => open(e)} title={ago(e.ts)}>{clockTime(e.ts)}</button>
{/snippet}
{#snippet statusCell(e: LogEntry)}<Badge tone={statusTone(e.status)} mono>{e.status}</Badge>{/snippet}
{#snippet modelCell(e: LogEntry)}
  <div class="flex min-w-0 flex-col">
    <span class="truncate font-mono text-sm">{e.model}</span>
    <span class="truncate text-xs text-fg-muted">
      {PROVIDERS[e.provider].name}{#if e.attempts.length > 1} · <span class="text-warning-fg">Fallback</span>{/if}{#if e.inputFormat === 'anthropic'} · Messages-API{/if}
    </span>
  </div>
{/snippet}
{#snippet keyCell(e: LogEntry)}<span class="truncate text-sm text-fg-muted">{e.keyName}</span>{/snippet}
{#snippet tokCell(e: LogEntry)}<span class="num text-sm">{num(e.inTok)} <span class="text-fg-subtle">→</span> {num(e.outTok)}</span>{/snippet}
{#snippet costCell(e: LogEntry)}<span class="num text-sm">{money(e.cost)}</span>{/snippet}
{#snippet latCell(e: LogEntry)}<span class={['num text-sm', e.latency > 4000 ? 'text-warning-fg' : '']}>{ms(e.latency)}</span>{/snippet}
{#snippet cacheCell(e: LogEntry)}
  {#if e.cache === 'hit'}<Badge tone="accent" icon="database">exakt</Badge>{:else if e.cache === 'semantic'}<Badge tone="info" icon="sparkles">semantisch</Badge>{:else}<span
      class="text-sm text-fg-subtle">–</span
    >{/if}
{/snippet}

<PageHeader title="Logs" description="Alle Requests über das Gateway – inklusive Fallback-Versuchen, Cache-Treffern und Kosten.">
  {#snippet badge()}
    {#if live && res.state === 'ok'}<Badge tone="success" dot>Live</Badge>{/if}
  {/snippet}
  {#snippet actions()}
    <Tooltip text={live ? 'Live-Aktualisierung anhalten' : 'Live-Aktualisierung fortsetzen'}>
      {#snippet trigger({ props })}
        <Button {...props} variant="secondary" icon={live ? 'clock' : 'play'} onclick={() => (live = !live)}>{live ? 'Pausieren' : 'Fortsetzen'}</Button>
      {/snippet}
    </Tooltip>
    <Button variant="secondary" icon="download">Export</Button>
  {/snippet}
</PageHeader>

{#if res.state === 'unavailable'}
  <Card><EmptyState icon="list" title="Noch keine Logs" text={res.reason} /></Card>
{:else}
  <div class="mb-4 grid grid-cols-2 gap-3 md:grid-cols-4" data-testid="log-stats">
    {#each [{ l: 'Requests (Auswahl)', v: num(stats.count), i: 'activity' }, { l: 'Fehlerquote', v: percent(stats.errors), i: 'alert' }, { l: 'p95-Latenz', v: ms(stats.p95), i: 'timer' }, { l: 'Kosten', v: money(stats.cost), i: 'coins' }] as const as s (s.l)}
      <div class="flex items-center gap-3 rounded-lg border border-line bg-surface px-4 py-3 shadow-card">
        <span class="grid size-8 place-items-center rounded-md bg-surface-2 text-fg-muted"><Icon name={s.i} size={16} /></span>
        <div class="min-w-0">
          <div class="truncate text-sm text-fg-muted">{s.l}</div>
          <div class="num text-lg font-semibold">{s.v}</div>
        </div>
      </div>
    {/each}
  </div>
  <Card flush testid="logs-card">
    <DataTable
      {rows}
      {columns}
      bind:q
      key={(e) => e.id}
      caption="Request-Logs"
      search={(e) => `${e.id} ${e.model} ${e.keyName} ${e.deployment} ${e.status}`}
      searchPlaceholder="Request-ID, Key, Modell …"
      sortId="ts"
      asc={false}
      perPage={25}
      dense
      onrowclick={open}
      selectedKey={detailOpen ? selected?.id : null}
      testid="logs-table"
      resetFilters={() => {
        status = 'all';
        model = 'all';
        cache = 'all';
      }}
    >
      {#snippet toolbar()}
        <div class="w-36">
          <Select
            size="sm"
            label="Status"
            bind:value={status}
            items={[
              { value: 'all', label: 'Alle Status' },
              { value: 'ok', label: 'Erfolgreich (2xx)' },
              { value: 'error', label: 'Fehler (4xx/5xx)' },
              { value: '429', label: 'Ratenlimit (429)' },
              { value: '5xx', label: 'Upstream (5xx)' },
            ]}
          />
        </div>
        <div class="w-48">
          <Select size="sm" label="Modell" bind:value={model} items={[{ value: 'all', label: 'Alle Modelle' }, ...MODEL_GROUPS.map((m) => ({ value: m.alias, label: m.alias }))]} />
        </div>
        <div class="hidden w-36 md:block">
          <Select
            size="sm"
            label="Cache"
            bind:value={cache}
            items={[
              { value: 'all', label: 'Cache: alle' },
              { value: 'hit', label: 'Treffer' },
              { value: 'miss', label: 'kein Treffer' },
            ]}
          />
        </div>
      {/snippet}
    </DataTable>
  </Card>
{/if}

<LogDetail entry={selected} bind:open={detailOpen} />
