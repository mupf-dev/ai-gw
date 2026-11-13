<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Log-Explorer gegen GET /request-logs (AIGW-46/77): serverseitige Filter und Cursor-Pagination (Status,
  // Fehlerklasse F47, Modellgruppe, Key, Cache inkl. cache-embedding F191, Stream, Request-/Trace-/Eltern-ID) –
  // Team/Organisation aus dem Kontext-Umschalter; Detail-Sheet mit Versuchen, Guardrails, Trace-Link und Payload.
  import { untrack } from 'svelte';
  import { amount, requestLogs, type RequestLogItem, type RequestLogQuery } from '../../lib/api/observe';
  import { providerLabel } from '../../lib/api/resources';
  import AsyncState from '../../lib/admin/AsyncState.svelte';
  import { Paged } from '../../lib/admin/paged.svelte';
  import { ctx } from '../../lib/context.svelte';
  import { ago, clockTime, ms, short } from '../../lib/format';
  import { navigate, route } from '../../lib/router.svelte';
  import Badge from '../../lib/ui/Badge.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../../lib/ui/DataTable.svelte';
  import EmptyState from '../../lib/ui/EmptyState.svelte';
  import PageHeader from '../../lib/ui/PageHeader.svelte';
  import Select from '../../lib/ui/Select.svelte';
  import { input } from '../../lib/ui/styles';
  import RequestDetail from './RequestDetail.svelte';

  const p = new URLSearchParams(route.search);
  const TEXT = ['modelGroup', 'key', 'deployment', 'requestId', 'traceId', 'parentRequestId', 'clientRequestId', 'errorClass'] as const;
  let f = $state({
    status: p.get('status') ?? '',
    cache: p.get('cache') ?? '',
    stream: p.get('stream') ?? '',
    ...(Object.fromEntries(TEXT.map((k) => [k, p.get(k) ?? ''])) as Record<(typeof TEXT)[number], string>),
  });
  let field = $state<(typeof TEXT)[number]>((TEXT.find((k) => p.get(k)) ?? 'requestId') as (typeof TEXT)[number]);
  let text = $state(p.get(field) ?? '');

  const query = (): RequestLogQuery => ({
    status: f.status || undefined,
    cache: f.cache || undefined,
    stream: f.stream === '' ? undefined : f.stream === 'true',
    team: ctx.teamId || undefined,
    organization: ctx.orgId || undefined,
    ...Object.fromEntries(TEXT.map((k) => [k, f[k].trim() || undefined])),
  });
  const logs = new Paged<RequestLogItem>((_q, limit, cursor) => requestLogs(query(), limit, cursor), () => ({}), { pageSize: 50 });
  let first = true;
  $effect(() => {
    void JSON.stringify(f);
    void ctx.teamId;
    void ctx.orgId;
    if (first) {
      first = false;
      return;
    }
    untrack(() => {
      const qs = new URLSearchParams(Object.entries(f).filter(([, v]) => v) as [string, string][]).toString();
      navigate(`/logs${qs ? `?${qs}` : ''}`, true);
      void logs.first();
    });
  });
  function applyText() {
    for (const k of TEXT) f[k] = k === field ? text : '';
  }
  function reset() {
    f = { status: '', cache: '', stream: '', ...(Object.fromEntries(TEXT.map((k) => [k, ''])) as Record<(typeof TEXT)[number], string>) };
    text = '';
  }

  let selected = $state<string | null>(p.get('requestId'));
  let detailOpen = $state(!!p.get('requestId'));
  const openDetail = (id: string) => {
    selected = id;
    detailOpen = true;
  };

  const FIELDS: { value: (typeof TEXT)[number]; label: string }[] = [
    { value: 'requestId', label: 'Request-ID' },
    { value: 'clientRequestId', label: 'Client-Request-ID' },
    { value: 'traceId', label: 'Trace-ID' },
    { value: 'parentRequestId', label: 'Eltern-Anfrage' },
    { value: 'modelGroup', label: 'Modellgruppe' },
    { value: 'deployment', label: 'Deployment' },
    { value: 'key', label: 'Key' },
    { value: 'errorClass', label: 'Fehlerklasse' },
  ];

  const columns: Column<RequestLogItem>[] = [
    { id: 'time', label: 'Zeit', cell: timeCell },
    { id: 'status', label: 'Status', cell: statusCell },
    { id: 'model', label: 'Modell', cell: modelCell },
    { id: 'key', label: 'Key', cell: keyCell, from: 'md' },
    { id: 'tokens', label: 'Tokens', cell: tokensCell, align: 'right', from: 'sm' },
    { id: 'cost', label: 'Kosten', cell: costCell, align: 'right', from: 'lg' },
    { id: 'latency', label: 'Latenz', cell: latencyCell, align: 'right' },
    { id: 'cache', label: 'Cache', cell: cacheCell, from: 'xl' },
  ];
</script>

{#snippet timeCell(e: RequestLogItem)}
  <button type="button" class="num text-left text-sm hover:text-accent-text" onclick={() => openDetail(e.id)} title={e.id}>{clockTime(Date.parse(e.at))}<span class="block text-xs text-fg-subtle">{ago(Date.parse(e.at))}</span></button>
{/snippet}
{#snippet statusCell(e: RequestLogItem)}
  <span class="flex flex-wrap items-center gap-1">
    <Badge tone={e.status >= 500 ? 'danger' : e.status >= 400 ? 'warning' : 'success'} mono>{e.status}</Badge>
    {#if e.errorClass}<span class="font-mono text-xs text-fg-muted">{e.errorClass}</span>{/if}
    {#if e.guardrails?.length}<Badge tone="warning" icon="shield">{e.guardrails.reduce((a, g) => a + g.count, 0)}</Badge>{/if}
  </span>
{/snippet}
{#snippet modelCell(e: RequestLogItem)}
  <span class="block font-mono text-sm">{e.modelGroup ?? e.modelRequested ?? '–'}</span>
  <span class="block text-xs text-fg-muted">{e.provider ? providerLabel(e.provider) : ''}{e.attemptCount > 1 ? ` · ${e.attemptCount} Versuche` : ''}{e.parentRequestId ? ' · Folgeanfrage' : ''}</span>
{/snippet}
{#snippet keyCell(e: RequestLogItem)}<span class="font-mono text-sm text-fg-muted">{e.apiKey ?? '–'}</span>{/snippet}
{#snippet tokensCell(e: RequestLogItem)}<span class="num text-sm">{short(e.inputTokens)} → {short(e.outputTokens)}</span>{/snippet}
{#snippet costCell(e: RequestLogItem)}<span class="num text-sm">{amount(e.cost)}</span>{/snippet}
{#snippet latencyCell(e: RequestLogItem)}<span class="num text-sm">{ms(e.latencyMs)}</span>{/snippet}
{#snippet cacheCell(e: RequestLogItem)}{#if e.cache && e.cache !== 'none'}<Badge tone={e.cache.startsWith('hit') ? 'success' : 'neutral'} mono>{e.cache}</Badge>{/if}{/snippet}

<PageHeader title="Logs" description="Einzelne Anfragen an das Gateway – neueste zuerst. Filter wirken auf dem Server; Team/Organisation aus dem Kontext oben.">
  {#snippet actions()}
    <Button icon="refresh" onclick={() => logs.first()} loading={logs.loading}>Aktualisieren</Button>
  {/snippet}
</PageHeader>

<Card flush testid="logs-card">
  <div class="flex flex-wrap items-center gap-2 border-b border-line px-3 py-2.5" data-testid="log-filters">
    <div class="w-36">
      <Select size="sm" label="Status" bind:value={f.status} items={[{ value: '', label: 'Alle Status' }, { value: 'success', label: 'Erfolg' }, { value: 'error', label: 'Fehler' }, { value: '4xx', label: '4xx' }, { value: '429', label: '429' }, { value: '5xx', label: '5xx' }]} />
    </div>
    <div class="w-40">
      <Select size="sm" label="Cache" bind:value={f.cache} items={[{ value: '', label: 'Cache: alle' }, { value: 'hit', label: 'Treffer' }, { value: 'hit-exact', label: 'Treffer exakt' }, { value: 'hit-semantic', label: 'Treffer semantisch' }, { value: 'miss', label: 'kein Treffer' }, { value: 'none', label: 'ohne Cache' }, { value: 'cache-embedding', label: 'Cache-Embedding' }]} />
    </div>
    <div class="w-32">
      <Select size="sm" label="Stream" bind:value={f.stream} items={[{ value: '', label: 'Stream: alle' }, { value: 'true', label: 'nur Streams' }, { value: 'false', label: 'keine Streams' }]} />
    </div>
    <div class="w-44"><Select size="sm" label="Suchfeld" bind:value={field} items={FIELDS} /></div>
    <input class={[input, 'h-8 w-56 text-sm']} aria-label="Suchwert" placeholder="exakter Wert, Enter" bind:value={text} onchange={applyText} onkeydown={(e) => e.key === 'Enter' && applyText()} data-testid="log-search" />
    <Button size="sm" variant="ghost" onclick={reset}>Zurücksetzen</Button>
  </div>
  <AsyncState loader={logs}>
    {#snippet children(rows)}
      <DataTable {rows} {columns} server={logs} filtered={Object.values(f).some(Boolean)} key={(e) => e.id} caption="Anfragen" dense testid="logs-table" onrowclick={(e) => openDetail(e.id)} resetFilters={reset}>
        {#snippet empty()}<EmptyState icon="list" title="Noch keine Anfragen" text="Sobald Clients das Gateway nutzen (und observe.request_log an ist), erscheinen die Anfragen hier." />{/snippet}
      </DataTable>
    {/snippet}
  </AsyncState>
</Card>

<RequestDetail bind:open={detailOpen} id={selected} onopen={openDetail} />
