<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Übersicht: KPI-Kacheln, Zeitreihe je Kennzahl, Top-Modelle, Anbieterzustand, letzte Fehler, Gateway-Info.
  import { Tabs, ToggleGroup } from 'bits-ui';
  import { currentOrg } from '../../lib/context.svelte';
  import { logs, overview } from '../../lib/data.svelte';
  import { ago, money, ms, num, percent, short } from '../../lib/format';
  import { PROVIDERS } from '../../lib/mock/data';
  import type { Health, RangeId, Series } from '../../lib/mock/types';
  import { href } from '../../lib/router.svelte';
  import Badge from '../../lib/ui/Badge.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import EmptyState from '../../lib/ui/EmptyState.svelte';
  import Icon, { type IconName } from '../../lib/ui/Icon.svelte';
  import KpiTile from '../../lib/ui/KpiTile.svelte';
  import Meter from '../../lib/ui/Meter.svelte';
  import PageHeader from '../../lib/ui/PageHeader.svelte';
  import { segment, segmentItem } from '../../lib/ui/styles';
  import TimeSeriesChart from '../../lib/ui/TimeSeriesChart.svelte';
  import { gateway, loadVersion } from '../../lib/version.svelte';

  loadVersion();

  let range = $state<RangeId>('24h');
  let metric = $state<keyof Omit<Series, 't'>>('requests');
  const res = $derived(overview(range));
  const data = $derived(res.state === 'ok' ? res.data : null);
  const errors = $derived.by(() => {
    const l = logs();
    return l.state === 'ok' ? l.data.filter((e) => e.status >= 400).slice(0, 5) : [];
  });

  const RANGES: { value: RangeId; label: string }[] = [
    { value: '24h', label: '24 Std.' },
    { value: '7d', label: '7 Tage' },
    { value: '30d', label: '30 Tage' },
  ];
  const METRICS: { id: keyof Omit<Series, 't'>; label: string; fmt: (v: number) => string }[] = [
    { id: 'requests', label: 'Requests', fmt: short },
    { id: 'tokens', label: 'Tokens', fmt: short },
    { id: 'cost', label: 'Kosten', fmt: money },
    { id: 'p95', label: 'p95-Latenz', fmt: ms },
    { id: 'errorRate', label: 'Fehlerquote', fmt: percent },
    { id: 'cacheRate', label: 'Cache-Treffer', fmt: percent },
  ];
  const current = $derived(METRICS.find((m) => m.id === metric) ?? METRICS[0]);
  const rel = (a: number, b: number) => (b ? (a - b) / b : 0);
  const HEALTH: Record<Health, { tone: 'success' | 'warning' | 'danger'; label: string; icon: IconName }> = {
    healthy: { tone: 'success', label: 'Betriebsbereit', icon: 'check-circle' },
    degraded: { tone: 'warning', label: 'Eingeschränkt', icon: 'alert' },
    down: { tone: 'danger', label: 'Ausgefallen', icon: 'x-circle' },
  };
  // Mini-Verläufe: letzte 24 Punkte
  const tail = (xs: number[]) => xs.slice(-24);
</script>

<PageHeader title="Übersicht" description="Verkehr, Kosten und Zustand des Gateways für {currentOrg().name}.">
  {#snippet actions()}
    <ToggleGroup.Root type="single" bind:value={range} class={segment} aria-label="Zeitraum">
      {#each RANGES as r (r.value)}
        <ToggleGroup.Item value={r.value} class={segmentItem}>{r.label}</ToggleGroup.Item>
      {/each}
    </ToggleGroup.Root>
    <Button icon="download" size="md" variant="secondary">Export</Button>
  {/snippet}
</PageHeader>

{#if !data}
  <div class="grid gap-4 lg:grid-cols-3">
    <Card class="lg:col-span-2">
      <EmptyState icon="activity" title="Noch keine Kennzahlen" text={res.state === 'unavailable' ? res.reason : ''} />
    </Card>
    {@render gatewayCard()}
  </div>
{:else}
  {@const k = data.kpis}
  {@const p = data.previous}
  <div class="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-6" data-testid="kpis">
    <KpiTile label="Requests" icon="activity" value={short(k.requests)} delta={rel(k.requests, p.requests)} trend={tail(data.series.requests)} />
    <KpiTile label="Tokens" icon="layers" value={short(k.tokens)} delta={rel(k.tokens, p.tokens)} trend={tail(data.series.tokens)} />
    <KpiTile label="Kosten" icon="coins" value={money(k.cost)} delta={rel(k.cost, p.cost)} better="down" trend={tail(data.series.cost)} />
    <KpiTile label="p95-Latenz" icon="timer" value={ms(k.p95)} delta={rel(k.p95, p.p95)} better="down" trend={tail(data.series.p95)} />
    <KpiTile label="Fehlerquote" icon="alert" value={percent(k.errorRate)} delta={rel(k.errorRate, p.errorRate)} better="down" trend={tail(data.series.errorRate)} />
    <KpiTile label="Cache-Treffer" icon="database" value={percent(k.cacheRate)} delta={rel(k.cacheRate, p.cacheRate)} trend={tail(data.series.cacheRate)} />
  </div>

  <Card class="mt-4" testid="main-chart">
    {#snippet actions()}
      <Tabs.Root bind:value={metric} class="w-full">
        <Tabs.List class="scrollbar-thin -mb-3 flex gap-1 overflow-x-auto" aria-label="Kennzahl">
          {#each METRICS as m (m.id)}
            <Tabs.Trigger
              value={m.id}
              class="h-10 shrink-0 border-b-2 border-transparent px-2.5 text-base font-medium text-fg-muted hover:text-fg data-[state=active]:border-accent data-[state=active]:text-fg"
            >
              {m.label}
            </Tabs.Trigger>
          {/each}
        </Tabs.List>
      </Tabs.Root>
    {/snippet}
    <div class="mb-3 flex flex-wrap items-baseline gap-x-3">
      <span class="num text-xl font-semibold text-fg">
        {current.id === 'p95' ? ms(k.p95) : current.id === 'errorRate' ? percent(k.errorRate) : current.id === 'cacheRate' ? percent(k.cacheRate) : current.id === 'cost' ? money(k.cost) : num(k[current.id])}
      </span>
      <span class="text-sm text-fg-muted">{current.label} · {RANGES.find((r) => r.value === range)?.label}</span>
    </div>
    <TimeSeriesChart
      t={data.series.t}
      series={[{ label: current.label, values: data.series[current.id], color: '--color-chart-1' }]}
      format={current.fmt}
      label="{current.label} im Zeitverlauf"
    />
  </Card>

  <div class="mt-4 grid gap-4 xl:grid-cols-5">
    <Card title="Top-Modellgruppen" description="nach Requests im Zeitraum" flush class="xl:col-span-3" testid="top-models">
      {#snippet actions()}
        <Button href={href('/models')} size="sm" variant="ghost" iconRight="chevron-right">Alle Modelle</Button>
      {/snippet}
      <div class="scrollbar-thin overflow-x-auto">
        <table class="w-full text-left text-base">
          <thead>
            <tr class="border-b border-line text-sm text-fg-muted">
              <th class="px-4 py-2 font-medium">Modellgruppe</th>
              <th class="px-4 py-2 text-right font-medium">Requests</th>
              <th class="hidden px-4 py-2 text-right font-medium sm:table-cell">Tokens</th>
              <th class="px-4 py-2 text-right font-medium">Kosten</th>
              <th class="hidden px-4 py-2 text-right font-medium md:table-cell">p95</th>
            </tr>
          </thead>
          <tbody>
            {#each data.topModels as m (m.alias)}
              <tr class="border-b border-line last:border-0">
                <td class="px-4 py-2.5"><a class="font-mono text-sm text-fg hover:text-accent-text" href={href(`/models?group=${m.alias}`)}>{m.alias}</a></td>
                <td class="num px-4 py-2.5 text-right">{short(m.requests)}</td>
                <td class="num hidden px-4 py-2.5 text-right text-fg-muted sm:table-cell">{short(m.tokens)}</td>
                <td class="num px-4 py-2.5 text-right">{money(m.cost)}</td>
                <td class="num hidden px-4 py-2.5 text-right text-fg-muted md:table-cell">{ms(m.p95)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </Card>

    <Card title="Anbieter" description="Zustand und Verkehrsanteil" class="xl:col-span-2" testid="providers">
      <ul class="flex flex-col gap-3">
        {#each data.providers as pr (pr.id)}
          <li class="grid grid-cols-[1fr_auto] items-center gap-x-3 gap-y-1">
            <span class="flex min-w-0 items-center gap-2">
              <span class="truncate text-base font-medium text-fg">{PROVIDERS[pr.id].name}</span>
              <Badge tone={HEALTH[pr.health].tone} dot>{HEALTH[pr.health].label}</Badge>
            </span>
            <span class="num text-sm text-fg-muted">{percent(pr.share)}</span>
            <Meter value={pr.share} label="Anteil {PROVIDERS[pr.id].name}" tone="chart" />
            <span class="num text-xs text-fg-subtle">p95 {ms(pr.p95)}</span>
          </li>
        {/each}
      </ul>
    </Card>
  </div>

  <div class="mt-4 grid gap-4 lg:grid-cols-3">
    <Card title="Letzte Fehler" flush class="lg:col-span-2" testid="recent-errors">
      {#snippet actions()}
        <Button href={href('/logs?status=error')} size="sm" variant="ghost" iconRight="chevron-right">Im Log-Explorer</Button>
      {/snippet}
      <ul>
        {#each errors as e (e.id)}
          <li class="flex items-center gap-3 border-b border-line px-4 py-2.5 last:border-0">
            <Badge tone={e.status === 429 ? 'warning' : 'danger'} mono>{e.status}</Badge>
            <span class="min-w-0 flex-1 truncate text-base">
              <span class="font-mono text-sm">{e.model}</span>
              <span class="text-fg-muted"> · {e.keyName}</span>
            </span>
            <span class="hidden truncate text-sm text-fg-muted sm:block">{(e.response as { error?: { type?: string } }).error?.type}</span>
            <span class="text-xs whitespace-nowrap text-fg-subtle">{ago(e.ts)}</span>
          </li>
        {/each}
      </ul>
    </Card>
    {@render gatewayCard()}
  </div>
{/if}

{#snippet gatewayCard()}
  <Card title="Gateway" testid="gateway-info">
    <dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-base">
      <dt class="text-fg-muted">Admin-API</dt>
      <dd>
        {#if gateway.info}<Badge tone="success" dot>verbunden</Badge>{:else if gateway.error}<Badge tone="danger" dot>nicht erreichbar</Badge
          >{:else}<Badge>prüfe …</Badge>{/if}
      </dd>
      <dt class="text-fg-muted">Version</dt>
      <dd class="num" data-testid="version">{gateway.info?.version ?? '–'}</dd>
      <dt class="text-fg-muted">Listener</dt>
      <dd class="font-mono text-sm">:4000 Daten · :4001 Steuerung</dd>
      <dt class="text-fg-muted">Konfiguration</dt>
      <dd class="flex items-center gap-1.5 text-sm"><Icon name="file-text" size={14} class="text-fg-muted" /> YAML (GitOps)</dd>
    </dl>
  </Card>
{/snippet}
