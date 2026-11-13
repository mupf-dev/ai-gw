<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Übersicht gegen die Analytics-API (AIGW-46/77): KPIs mit Vorperiode (Quoten in Prozentpunkten, F144), Zeitreihe
  // je Kennzahl (uPlot), Kosten-Treemap nach Team bzw. Modellgruppe (LayerChart, lazy, F18), Top-Modellgruppen,
  // Anbieter-Anteile, Fehler nach Klasse (F47), letzte Fehler aus dem Log-Explorer, Gateway-Info.
  import { Tabs, ToggleGroup } from 'bits-ui';
  import { amount, lastHours, requestLogs, summary, timeseries, top, type Dimension, type Kpi, type Metric } from '../../lib/api/observe';
  import { providerLabel } from '../../lib/api/resources';
  import AsyncState from '../../lib/admin/AsyncState.svelte';
  import { Loader } from '../../lib/admin/errors.svelte';
  import { RANGES, type RangeId } from '../../lib/admin/UsagePanel.svelte';
  import { ctx } from '../../lib/context.svelte';
  import { ago, ms, percent, short } from '../../lib/format';
  import { detailPath } from '../../lib/nav';
  import { href } from '../../lib/router.svelte';
  import Badge from '../../lib/ui/Badge.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import EmptyState from '../../lib/ui/EmptyState.svelte';
  import KpiTile from '../../lib/ui/KpiTile.svelte';
  import Meter from '../../lib/ui/Meter.svelte';
  import PageHeader from '../../lib/ui/PageHeader.svelte';
  import { segment, segmentItem } from '../../lib/ui/styles';
  import TimeSeriesChart from '../../lib/ui/TimeSeriesChart.svelte';
  import { gateway, loadVersion } from '../../lib/version.svelte';

  loadVersion();

  let range = $state<RangeId>('24h');
  let metric = $state<Metric>('requests');
  let treeBy = $state<Dimension>('team');
  const scope = () => ({ organization: ctx.orgId || undefined, team: ctx.teamId || undefined });
  const rng = () => lastHours(RANGES.find((r) => r.value === range)?.hours ?? 24);

  const base = new Loader(async () => {
    const r = rng();
    const f = scope();
    const [s, models, providers, errorsByClass, recent] = await Promise.all([
      summary(r, f),
      top('model_group', r, f, { metric: 'requests', limit: 8 }),
      top('provider', r, f, { metric: 'requests', limit: 6 }),
      top('error_class', r, f, { metric: 'errors', limit: 6 }),
      requestLogs({ ...f, status: 'error', from: r.from }, 5),
    ]);
    return { s, models, providers, errorsByClass, recent: recent.items };
  });
  const series = new Loader(() => timeseries(metric, rng(), scope()));
  const tree = new Loader(() => top(treeBy, rng(), scope(), { metric: 'cost', limit: 12 }));
  let first = true;
  $effect(() => {
    void range;
    void ctx.orgId;
    void ctx.teamId;
    if (first) {
      first = false;
      return;
    }
    void base.reload();
    void series.reload();
    void tree.reload();
  });
  let firstM = true;
  $effect(() => {
    void metric;
    if (firstM) {
      firstM = false;
      return;
    }
    void series.reload();
  });
  let firstT = true;
  $effect(() => {
    void treeBy;
    if (firstT) {
      firstT = false;
      return;
    }
    void tree.reload();
  });

  const METRICS: { id: Metric; label: string; fmt: (v: number) => string }[] = [
    { id: 'requests', label: 'Requests', fmt: short },
    { id: 'tokens', label: 'Tokens', fmt: short },
    { id: 'cost', label: 'Kosten', fmt: (v) => amount(v) },
    { id: 'latency_p95', label: 'p95-Latenz', fmt: ms },
    { id: 'error_rate', label: 'Fehlerquote', fmt: percent },
    { id: 'cache_hit_rate', label: 'Cache-Treffer', fmt: percent },
  ];
  const current = $derived(METRICS.find((m) => m.id === metric) ?? METRICS[0]);
  const v = (k: Kpi) => k.value ?? 0;
  const d = (k: Kpi) => (k.deltaPercent == null ? undefined : k.deltaPercent / 100);
  const dp = (k: Kpi) => (k.deltaPoints == null ? undefined : k.deltaPoints / 100);
  const treemap = () => import('../../lib/charts/CostTreemap.svelte');
  const ERROR_LABEL: Record<string, string> = {
    rate_limit: 'Ratenlimit (Anbieter)',
    rate_limited: 'Ratenlimit',
    timeout: 'Zeitüberschreitung',
    connect: 'Verbindung',
    upstream_5xx: 'Anbieter 5xx',
    auth: 'Authentifizierung',
    budget_exceeded: 'Budget erschöpft',
    content_filter: 'Inhaltsfilter',
    context_length: 'Kontextlänge',
    invalid_request: 'Ungültige Anfrage',
  };
</script>

<PageHeader title="Übersicht" description="Verkehr, Kosten und Zustand des Gateways{ctx.teamId ? ` für Team ${ctx.teamId}` : ctx.orgId ? ` für ${ctx.orgId}` : ''}.">
  {#snippet actions()}
    <ToggleGroup.Root type="single" value={range} onValueChange={(x) => x && (range = x as RangeId)} class={segment} aria-label="Zeitraum">
      {#each RANGES as r (r.value)}
        <ToggleGroup.Item value={r.value} class={segmentItem}>{r.label}</ToggleGroup.Item>
      {/each}
    </ToggleGroup.Root>
  {/snippet}
</PageHeader>

<AsyncState loader={base}>
  {#snippet children({ s, models, providers, errorsByClass, recent })}
    {#if !s.rollups}
      <p class="mb-4 rounded-md border border-info-fg/30 bg-info-bg px-3 py-2 text-sm text-info-fg" role="status" data-testid="rollups-off">
        Auswertungen sind abgeschaltet (<span class="font-mono">observe.rollups: false</span>) – die Dashboards bleiben leer. Einzelanfragen stehen weiter im Log-Explorer.
      </p>
    {/if}
    <div class="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-6" data-testid="kpis">
      <KpiTile label="Requests" icon="activity" value={short(v(s.requests))} delta={d(s.requests)} />
      <KpiTile label="Tokens" icon="layers" value={short(v(s.tokens))} delta={d(s.tokens)} />
      <KpiTile label="Kosten" icon="coins" value={amount(s.cost.amount ?? v(s.cost))} delta={d(s.cost)} better="down" />
      <KpiTile label="p95-Latenz" icon="timer" value={s.latencyP95Ms.value == null ? '–' : ms(v(s.latencyP95Ms))} delta={d(s.latencyP95Ms)} better="down" />
      <KpiTile label="Fehlerquote" icon="alert" value={s.errorRate.value == null ? '–' : percent(v(s.errorRate))} delta={dp(s.errorRate)} deltaUnit="pp" better="down" />
      <KpiTile label="Cache-Treffer" icon="database" value={s.cacheHitRate.value == null ? '–' : percent(v(s.cacheHitRate))} delta={dp(s.cacheHitRate)} deltaUnit="pp" />
    </div>

    <Card class="mt-4" testid="main-chart">
      {#snippet actions()}
        <Tabs.Root bind:value={metric} class="w-full">
          <Tabs.List class="scrollbar-thin -mb-3 flex gap-1 overflow-x-auto" aria-label="Kennzahl">
            {#each METRICS as m (m.id)}
              <Tabs.Trigger value={m.id} class="h-10 shrink-0 border-b-2 border-transparent px-2.5 text-base font-medium text-fg-muted hover:text-fg data-[state=active]:border-accent data-[state=active]:text-fg">{m.label}</Tabs.Trigger>
            {/each}
          </Tabs.List>
        </Tabs.Root>
      {/snippet}
      <AsyncState loader={series} rows={6}>
        {#snippet children(ts)}
          {@const pts = ts.series[0]?.points ?? []}
          <TimeSeriesChart t={pts.map((p) => Date.parse(p.t) / 1000)} series={[{ label: current.label, values: pts.map((p) => p.v ?? 0), color: '--color-chart-1' }]} format={current.fmt} label="{current.label} im Zeitverlauf (je {ts.step})" />
        {/snippet}
      </AsyncState>
    </Card>

    <div class="mt-4 grid gap-4 xl:grid-cols-5">
      <Card title="Kosten nach {treeBy === 'team' ? 'Team' : 'Modellgruppe'}" description="Fläche = Anteil an den Kosten im Zeitraum" class="xl:col-span-3" testid="cost-card">
        {#snippet actions()}
          <ToggleGroup.Root type="single" value={treeBy} onValueChange={(x) => x && (treeBy = x as Dimension)} class={segment} aria-label="Gruppierung">
            <ToggleGroup.Item value="team" class={segmentItem}>Team</ToggleGroup.Item>
            <ToggleGroup.Item value="model_group" class={segmentItem}>Modellgruppe</ToggleGroup.Item>
          </ToggleGroup.Root>
        {/snippet}
        <AsyncState loader={tree} rows={4}>
          {#snippet children(t)}
            {@const items = t.items.filter((i) => (i.value ?? 0) > 0).map((i) => ({ key: i.key, label: i.name || (i.key === '' ? 'ohne Team' : i.key), value: i.value ?? 0, text: amount(i.amount ?? i.value) }))}
            {#if items.length === 0}
              <EmptyState compact icon="coins" title="Keine Kosten im Zeitraum" text="Kosten entstehen mit Preisen für die genutzten Anbieter-Modelle (Preise)." />
            {:else}
              {#await treemap()}
                <div class="h-56 animate-pulse rounded-md bg-skeleton"></div>
              {:then mod}
                {@const Treemap = mod.default}
                <Treemap {items} label="Kosten nach {treeBy === 'team' ? 'Team' : 'Modellgruppe'}" />
              {/await}
              <ul class="sr-only">{#each items as i (i.key)}<li>{i.label}: {i.text}</li>{/each}</ul>
            {/if}
          {/snippet}
        </AsyncState>
      </Card>
      <Card title="Anbieter" description="Anteil an den Requests" class="xl:col-span-2" testid="providers">
        <ul class="flex flex-col gap-3">
          {#each providers.items as pr (pr.key)}
            <li class="grid grid-cols-[1fr_auto] items-center gap-x-3 gap-y-1">
              <span class="truncate text-base font-medium text-fg">{providerLabel(pr.key)}</span>
              <span class="num text-sm text-fg-muted">{percent(pr.share ?? 0)} · {short(pr.requests)}</span>
              <Meter value={pr.share ?? 0} label="Anteil {providerLabel(pr.key)}" tone="chart" class="col-span-2" />
            </li>
          {:else}
            <li class="text-sm text-fg-muted">Noch kein Verkehr im Zeitraum.</li>
          {/each}
        </ul>
      </Card>
    </div>

    <div class="mt-4 grid gap-4 xl:grid-cols-5">
      <Card title="Top-Modellgruppen" description="nach Requests im Zeitraum" flush class="xl:col-span-3" testid="top-models">
        {#snippet actions()}<Button href={href('/models')} size="sm" variant="ghost" iconRight="chevron-right">Alle Modelle</Button>{/snippet}
        {#if models.items.length === 0}
          <EmptyState compact icon="route" title="Noch kein Verkehr" text="Sobald Anfragen über das Gateway laufen, erscheinen sie hier." />
        {:else}
          <table class="w-full text-left text-base">
            <thead><tr class="border-b border-line text-sm text-fg-muted"><th class="px-4 py-2 font-medium">Modellgruppe</th><th class="px-4 py-2 text-right font-medium">Requests</th><th class="px-4 py-2 text-right font-medium">Anteil</th></tr></thead>
            <tbody>
              {#each models.items as m (m.key)}
                <tr class="border-b border-line last:border-0">
                  <td class="px-4 py-2.5">{#if m.key}<a class="font-mono text-sm text-fg hover:text-accent-text" href={href(detailPath('model-group', m.key))}>{m.name ?? m.key}</a>{:else}<span class="text-sm text-fg-muted">ohne Modellgruppe</span>{/if}</td>
                  <td class="num px-4 py-2.5 text-right">{short(m.requests)}</td>
                  <td class="num px-4 py-2.5 text-right text-fg-muted">{percent(m.share ?? 0)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      </Card>
      <Card title="Fehler nach Klasse" description="Anzahl im Zeitraum" class="xl:col-span-2" testid="error-classes">
        <ul class="flex flex-col gap-2">
          {#each errorsByClass.items.filter((e) => e.key) as e (e.key)}
            <li class="flex items-center gap-2 text-sm">
              <a class="min-w-0 flex-1 truncate hover:text-accent-text" href={href(`/logs?errorClass=${encodeURIComponent(e.key)}`)}>{ERROR_LABEL[e.key] ?? e.key} <span class="font-mono text-xs text-fg-subtle">{e.key}</span></a>
              <span class="num">{short(e.value ?? 0)}</span>
            </li>
          {:else}
            <li class="text-sm text-fg-muted">Keine Fehler im Zeitraum.</li>
          {/each}
        </ul>
      </Card>
    </div>

    <div class="mt-4 grid gap-4 lg:grid-cols-3">
      <Card title="Letzte Fehler" flush class="lg:col-span-2" testid="recent-errors">
        {#snippet actions()}<Button href={href('/logs?status=error')} size="sm" variant="ghost" iconRight="chevron-right">Im Log-Explorer</Button>{/snippet}
        <ul>
          {#each recent as e (e.id)}
            <li class="flex items-center gap-3 border-b border-line px-4 py-2.5 last:border-0">
              <Badge tone={e.status === 429 ? 'warning' : 'danger'} mono>{e.status}</Badge>
              <a class="min-w-0 flex-1 truncate text-base hover:text-accent-text" href={href(`/logs?requestId=${encodeURIComponent(e.id)}`)}>
                <span class="font-mono text-sm">{e.modelGroup ?? e.modelRequested ?? '–'}</span><span class="text-fg-muted"> · {e.apiKey ?? 'ohne Key'}</span>
              </a>
              <span class="hidden truncate font-mono text-xs text-fg-muted sm:block">{e.errorClass ?? e.errorCode ?? ''}</span>
              <span class="text-xs whitespace-nowrap text-fg-subtle">{ago(Date.parse(e.at))}</span>
            </li>
          {:else}
            <li class="px-4 py-3 text-sm text-fg-muted">Keine Fehler im Zeitraum.</li>
          {/each}
        </ul>
      </Card>
      <Card title="Gateway" testid="gateway-info">
        <dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-base">
          <dt class="text-fg-muted">Admin-API</dt>
          <dd>{#if gateway.info}<Badge tone="success" dot>verbunden</Badge>{:else if gateway.error}<Badge tone="danger" dot>nicht erreichbar</Badge>{:else}<Badge>prüfe …</Badge>{/if}</dd>
          <dt class="text-fg-muted">Version</dt>
          <dd class="num" data-testid="version">{gateway.info?.version ?? '–'}</dd>
          <dt class="text-fg-muted">Konfiguration</dt>
          <dd class="num text-sm">{#if gateway.info?.snapshot}Version {gateway.info.snapshot.version} · {gateway.info.snapshot.stale ? 'veraltet' : 'aktuell'}{:else}–{/if}</dd>
        </dl>
      </Card>
    </div>
  {/snippet}
</AsyncState>
