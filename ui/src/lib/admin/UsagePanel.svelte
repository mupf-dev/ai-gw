<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  export type RangeId = '24h' | '7d' | '30d';
  export const RANGES: { value: RangeId; label: string; hours: number }[] = [
    { value: '24h', label: '24 Std.', hours: 24 },
    { value: '7d', label: '7 Tage', hours: 168 },
    { value: '30d', label: '30 Tage', hours: 720 },
  ];
</script>

<script lang="ts">
  // Nutzung eines Objekts bzw. des ganzen Gateways (Analytics-API, AIGW-46): KPIs mit Vorperiode, Anfragen und
  // Kosten im Zeitverlauf (uPlot), Tokens nach Art gestapelt (LayerChart, lazy). Filter = Objekt der Detailseite.
  import { ToggleGroup } from 'bits-ui';
  import { lastHours, summary, timeseries, amount, type Kpi, type ScopeFilter } from '../api/observe';
  import { ms, percent, short } from '../format';
  import { href } from '../router.svelte';
  import Button from '../ui/Button.svelte';
  import Card from '../ui/Card.svelte';
  import KpiTile from '../ui/KpiTile.svelte';
  import { segment, segmentItem } from '../ui/styles';
  import TimeSeriesChart from '../ui/TimeSeriesChart.svelte';
  import AsyncState from './AsyncState.svelte';
  import { Loader } from './errors.svelte';

  let { filter = {}, logsQuery = '', testid = 'usage' }: { filter?: ScopeFilter; logsQuery?: string; testid?: string } = $props();

  let range = $state<RangeId>('24h');
  const data = new Loader(async () => {
    const r = lastHours(RANGES.find((x) => x.value === range)?.hours ?? 24);
    const [s, req, cost, inp, out] = await Promise.all([
      summary(r, filter),
      timeseries('requests', r, filter),
      timeseries('cost', r, filter),
      timeseries('input_tokens', r, filter),
      timeseries('output_tokens', r, filter),
    ]);
    return { s, req, cost, inp, out };
  });
  let first = true;
  $effect(() => {
    void range;
    if (first) {
      first = false;
      return;
    }
    void data.reload();
  });

  const v = (k: Kpi | undefined) => k?.value ?? 0;
  const d = (k: Kpi | undefined) => (k?.deltaPercent == null ? undefined : k.deltaPercent / 100);
  // Quoten: Änderung in Prozentpunkten (deltaPoints, F144).
  const dp = (k: Kpi | undefined) => (k?.deltaPoints == null ? undefined : k.deltaPoints / 100);
  const pts = (ts: { series: { points: { t: string; v?: number | null }[] }[] }) => ts.series[0]?.points ?? [];
  const charts = () => import('../charts/StackedArea.svelte');
</script>

<div class="flex flex-col gap-4" data-testid={testid}>
  <div class="flex flex-wrap items-center gap-2">
    <ToggleGroup.Root type="single" value={range} onValueChange={(x) => x && (range = x as RangeId)} class={segment} aria-label="Zeitraum">
      {#each RANGES as r (r.value)}
        <ToggleGroup.Item value={r.value} class={segmentItem}>{r.label}</ToggleGroup.Item>
      {/each}
    </ToggleGroup.Root>
    {#if logsQuery}
      <Button class="ml-auto" size="sm" variant="ghost" iconRight="chevron-right" href={href(`/logs?${logsQuery}`)}>Anfragen im Log-Explorer</Button>
    {/if}
  </div>
  <AsyncState loader={data}>
    {#snippet children({ s, req, cost, inp, out })}
      {#if !s.rollups}
        <p class="rounded-md border border-info-fg/30 bg-info-bg px-3 py-2 text-sm text-info-fg" role="status" data-testid="rollups-off">
          Auswertungen sind abgeschaltet (<span class="font-mono">observe.rollups: false</span>) – Kennzahlen und Diagramme bleiben leer. Einzelanfragen stehen weiter im Log-Explorer.
        </p>
      {/if}
      <div class="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-6" data-testid="usage-kpis">
        <KpiTile label="Requests" icon="activity" value={short(v(s.requests))} delta={d(s.requests)} />
        <KpiTile label="Tokens" icon="layers" value={short(v(s.tokens))} delta={d(s.tokens)} />
        <KpiTile label="Kosten" icon="coins" value={amount(s.cost.amount ?? v(s.cost))} delta={d(s.cost)} better="down" />
        <KpiTile label="p95-Latenz" icon="timer" value={s.latencyP95Ms.value == null ? '–' : ms(v(s.latencyP95Ms))} delta={d(s.latencyP95Ms)} better="down" />
        <KpiTile label="Fehlerquote" icon="alert" value={s.errorRate.value == null ? '–' : percent(v(s.errorRate))} delta={dp(s.errorRate)} deltaUnit="pp" better="down" />
        <KpiTile label="Cache-Treffer" icon="database" value={s.cacheHitRate.value == null ? '–' : percent(v(s.cacheHitRate))} delta={dp(s.cacheHitRate)} deltaUnit="pp" />
      </div>
      <div class="grid gap-4 xl:grid-cols-2">
        <Card title="Requests und Kosten" description="je {req.step}">
          <TimeSeriesChart
            t={pts(req).map((p) => Date.parse(p.t) / 1000)}
            series={[{ label: 'Requests', values: pts(req).map((p) => p.v ?? 0), color: '--color-chart-1' }]}
            format={short}
            label="Requests im Zeitverlauf"
            height={180}
          />
          <div class="mt-3">
            <TimeSeriesChart
              t={pts(cost).map((p) => Date.parse(p.t) / 1000)}
              series={[{ label: 'Kosten', values: pts(cost).map((p) => p.v ?? 0), color: '--color-chart-2' }]}
              format={(x) => amount(x)}
              label="Kosten im Zeitverlauf"
              height={140}
            />
          </div>
        </Card>
        <Card title="Tokens nach Art" description="Eingabe und Ausgabe gestapelt">
          {#await charts()}
            <div class="h-56 animate-pulse rounded-md bg-skeleton"></div>
          {:then mod}
            {@const Stacked = mod.default}
            <Stacked
              t={pts(inp).map((p) => Date.parse(p.t) / 1000)}
              series={[
                { key: 'input', label: 'Eingabe', values: pts(inp).map((p) => p.v ?? 0) },
                { key: 'output', label: 'Ausgabe', values: pts(out).map((p) => p.v ?? 0) },
              ]}
              label="Tokens nach Art im Zeitverlauf"
            />
          {/await}
        </Card>
      </div>
    {/snippet}
  </AsyncState>
</div>
