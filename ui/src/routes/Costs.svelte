<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Kostenanalyse (AIGW-117): Kennzahlen mit Vorperiode, Verlauf gestapelt nach wählbarer Dimension (Analytics-Zeitreihe
  // metric=cost, LayerChart lazy + Tabellen-Alternative), Aufschlüsselung nach Token-Art, Ersparnisse (Anbieter-Cache exakt,
  // Gateway-Cache als Schätzung), Plugin-Kosten, Kostentreiber mit Drilldown (Filter bzw. Log-Explorer), Budgets mit
  // Prognose bis Periodenende und Burn-down, Chargeback-Export (CSV/JSON). Filter stehen in der URL (teilbar).
  import { ToggleGroup } from 'bits-ui';
  import { untrack } from 'svelte';
  import {
    amount,
    budgetForecasts,
    costDrivers,
    costExport,
    costSummary,
    lastHours,
    timeseries,
    type BudgetForecast,
    type CostDriver,
    type Dimension,
    type ExportFormat,
    type Granularity,
    type ScopeFilter,
  } from '../lib/api/observe';
  import { providerLabel } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import { download, handleError, Loader } from '../lib/admin/errors.svelte';
  import { ctx } from '../lib/context.svelte';
  import { dateTime, day, percent, short } from '../lib/format';
  import { href, navigate, route } from '../lib/router.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import KpiTile from '../lib/ui/KpiTile.svelte';
  import Meter from '../lib/ui/Meter.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Select from '../lib/ui/Select.svelte';
  import { segment, segmentItem } from '../lib/ui/styles';
  import TimeSeriesChart from '../lib/ui/TimeSeriesChart.svelte';

  type CostDim = Exclude<Dimension, 'error_class'>;
  const DIMS: { value: CostDim; label: string; param: keyof ScopeFilter }[] = [
    { value: 'team', label: 'Team', param: 'team' },
    { value: 'org', label: 'Organisation', param: 'organization' },
    { value: 'model_group', label: 'Modellgruppe', param: 'modelGroup' },
    { value: 'deployment', label: 'Deployment', param: 'deployment' },
    { value: 'provider', label: 'Anbieter', param: 'provider' },
    { value: 'key', label: 'API-Key', param: 'key' },
  ];
  const RANGES = [
    { value: '24h', label: '24 Std.', hours: 24 },
    { value: '7d', label: '7 Tage', hours: 168 },
    { value: '30d', label: '30 Tage', hours: 720 },
    { value: '90d', label: '90 Tage', hours: 2160 },
  ] as const;
  type RangeId = (typeof RANGES)[number]['value'];
  const FILTER_KEYS: (keyof ScopeFilter)[] = ['organization', 'team', 'modelGroup', 'deployment', 'provider', 'key'];
  const FILTER_LABEL: Partial<Record<keyof ScopeFilter, string>> = {
    organization: 'Organisation',
    team: 'Team',
    modelGroup: 'Modellgruppe',
    deployment: 'Deployment',
    provider: 'Anbieter',
    key: 'API-Key',
  };

  // Zustand aus der URL (teilbar, Drilldown per Link).
  const qp = new URLSearchParams(route.search);
  let range = $state<RangeId>((RANGES.find((r) => r.value === qp.get('range'))?.value ?? '30d') as RangeId);
  let groupBy = $state<CostDim>((DIMS.find((d) => d.value === qp.get('groupBy'))?.value ?? 'team') as CostDim);
  let step = $state(qp.get('step') ?? '');
  let filters = $state<ScopeFilter>(Object.fromEntries(FILTER_KEYS.filter((k) => qp.get(k)).map((k) => [k, qp.get(k)!])));

  const dimLabel = (d: CostDim) => DIMS.find((x) => x.value === d)?.label ?? d;
  const rng = () => lastHours(RANGES.find((r) => r.value === range)?.hours ?? 720);
  // Kontext-Umschalter (Organisation/Team) als Grundfilter, ausdrückliche Filter haben Vorrang.
  const scope = (): ScopeFilter => ({ organization: ctx.orgId || undefined, team: ctx.teamId || undefined, ...filters });
  const stepOk = (s: string) => !(s === '1h' && range === '90d');

  const overview = new Loader(async () => {
    const r = rng();
    const [s, drivers] = await Promise.all([costSummary(r, scope()), costDrivers(groupBy, r, scope(), 10)]);
    return { s, drivers };
  });
  const series = new Loader(() =>
    timeseries('cost', rng(), scope(), { groupBy, limit: 5, ...(step && stepOk(step) ? { step: step as '1h' | '1d' } : {}) }),
  );
  const budgets = new Loader(() => budgetForecasts());

  // Neu laden bei Änderungen; URL mitführen.
  let first = true;
  $effect(() => {
    void [range, groupBy, step, JSON.stringify(filters), ctx.orgId, ctx.teamId];
    if (first) {
      first = false;
      return;
    }
    untrack(() => {
      const q = new URLSearchParams({ range, groupBy, ...(step ? { step } : {}), ...(filters as Record<string, string>) });
      navigate(`/costs?${q}`, true);
      void overview.reload();
      void series.reload();
    });
  });

  // Burn-down des gewählten Budgets.
  let selected = $state<{ level: BudgetForecast['budget']['level']; target: string } | null>(null);
  const burn = new Loader(async () => (selected ? (await budgetForecasts({ level: selected.level, target: selected.target })).items[0] ?? null : null));
  let firstB = true;
  $effect(() => {
    void selected;
    if (firstB) {
      firstB = false;
      return;
    }
    untrack(() => void burn.reload());
  });

  function drill(d: CostDim, item: CostDriver) {
    const param = DIMS.find((x) => x.value === d)!.param;
    filters = { ...filters, [param]: item.name ?? item.key };
    // Nächste sinnvolle Ebene: Team → Key, Organisation → Team, Modellgruppe → Deployment, sonst Modellgruppe.
    const next: Partial<Record<CostDim, CostDim>> = { org: 'team', team: 'key', model_group: 'deployment', provider: 'deployment', deployment: 'key', key: 'model_group' };
    groupBy = next[d] ?? 'model_group';
  }
  function unfilter(k: keyof ScopeFilter) {
    const f = { ...filters };
    delete f[k];
    filters = f;
  }
  // Log-Explorer kennt Modellgruppe, Deployment und Key als Filter.
  function logsLink(d: CostDim, item: CostDriver): string | null {
    const p = d === 'model_group' ? 'modelGroup' : d === 'deployment' ? 'deployment' : d === 'key' ? 'key' : null;
    return p && item.key ? href(`/logs?${p}=${encodeURIComponent(item.name ?? item.key)}`) : null;
  }
  const groupName = (d: CostDim, key: string, name?: string | null) =>
    key === '__other__' ? 'Übrige' : key === '' ? (d === 'team' ? 'ohne Team' : d === 'key' ? 'ohne Key' : '–') : d === 'provider' ? providerLabel(key) : (name ?? key);

  // Export
  let expDim = $state<CostDim>('team');
  let expGran = $state<Granularity>('month');
  let expFormat = $state<ExportFormat>('csv');
  let exporting = $state(false);
  async function runExport() {
    exporting = true;
    try {
      const r = rng();
      const text = await costExport({ dimension: expDim, granularity: expGran, format: expFormat, ...r, ...scope() });
      download(`aigw-kosten-${expDim}-${r.from.slice(0, 10)}-${r.to.slice(0, 10)}.${expFormat}`, text, expFormat === 'csv' ? 'text/csv' : 'application/json');
    } catch (e) {
      handleError(e, { action: 'Kosten-Export' });
    } finally {
      exporting = false;
    }
  }

  const PART_LABEL: Record<string, string> = {
    input: 'Eingabe',
    cache_read: 'Cache lesen',
    cache_write: 'Cache schreiben',
    output: 'Ausgabe',
    plugins: 'Plugins',
    other: 'Sonstiges / nicht aufgeschlüsselt',
  };
  const HEALTH: Record<string, { tone: 'success' | 'warning' | 'danger' | 'neutral'; label: string }> = {
    ok: { tone: 'success', label: 'im Plan' },
    warning: { tone: 'warning', label: 'Warnschwelle' },
    at_risk: { tone: 'danger', label: 'Überschreitung erwartet' },
    exhausted: { tone: 'danger', label: 'erschöpft' },
  };
  const LEVEL: Record<string, string> = { organization: 'Organisation', team: 'Team', 'api-key': 'Key' };
  const PERIOD: Record<string, string> = { day: 'Tag', week: 'Woche', month: 'Monat', total: 'gesamt' };
  const n = (v: string | number | null | undefined) => (v == null ? 0 : Number(v));
  const deltaFrac = (pct: number | null | undefined) => (pct == null ? undefined : pct / 100);
  const signed = (v: string) => (n(v) > 0 ? `+${amount(v)}` : n(v) < 0 ? `−${amount(Math.abs(n(v)))}` : amount(0));
  const stacked = () => import('../lib/charts/StackedArea.svelte');
</script>

<PageHeader title="Kosten" description="Kostenanalyse aus den Rollups: Verlauf, Treiber, Ersparnisse, Budgets und Chargeback-Export.">
  {#snippet actions()}
    <ToggleGroup.Root type="single" value={range} onValueChange={(x) => x && (range = x as RangeId)} class={segment} aria-label="Zeitraum">
      {#each RANGES as r (r.value)}
        <ToggleGroup.Item value={r.value} class={segmentItem}>{r.label}</ToggleGroup.Item>
      {/each}
    </ToggleGroup.Root>
  {/snippet}
</PageHeader>

{#if Object.keys(filters).length}
  <div class="mb-4 flex flex-wrap items-center gap-2" data-testid="cost-filters">
    <span class="text-sm text-fg-muted">Gefiltert:</span>
    {#each FILTER_KEYS.filter((k) => filters[k]) as k (k)}
      <span class="inline-flex items-center gap-1 rounded-md border border-line bg-surface-2 py-0.5 pr-1 pl-2 text-sm">
        {FILTER_LABEL[k]}: <span class="font-mono">{filters[k]}</span>
        <button type="button" class="grid size-6 place-items-center rounded text-fg-muted hover:text-fg" aria-label="Filter {FILTER_LABEL[k]} entfernen" onclick={() => unfilter(k)}><Icon name="x" size={14} /></button>
      </span>
    {/each}
    <Button size="sm" variant="ghost" onclick={() => (filters = {})}>Alle entfernen</Button>
  </div>
{/if}

<AsyncState loader={overview}>
  {#snippet children({ s, drivers })}
    {#if !s.rollups}
      <p class="mb-4 rounded-md border border-info-fg/30 bg-info-bg px-3 py-2 text-sm text-info-fg" role="status" data-testid="rollups-off">
        Auswertungen sind abgeschaltet (<span class="font-mono">observe.rollups: false</span>) – die Kostenanalyse bleibt leer.
      </p>
    {/if}
    {@const cur = s.currency}
    <div class="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-6" data-testid="cost-kpis">
      <KpiTile label="Gesamtkosten" icon="coins" value={amount(s.total.amount)} unit={cur} delta={deltaFrac(s.total.deltaPercent)} better="down" hint="Vorperiode {amount(s.total.previous)} {cur}" testid="kpi-total" />
      <KpiTile label="je Anfrage" icon="activity" value={s.costPerRequest ? amount(s.costPerRequest.amount, 4) : '–'} unit={cur} delta={deltaFrac(s.costPerRequest?.deltaPercent)} better="down" hint="{short(s.requests)} Anfragen" />
      <KpiTile label="je 1k Eingabe-Tokens" icon="arrow-up" value={s.costPer1kInputTokens ? amount(s.costPer1kInputTokens, 4) : '–'} unit={cur} hint="{short(s.inputTokens)} Tokens" />
      <KpiTile label="je 1k Ausgabe-Tokens" icon="arrow-down" value={s.costPer1kOutputTokens ? amount(s.costPer1kOutputTokens, 4) : '–'} unit={cur} hint="{short(s.outputTokens)} Tokens" />
      <KpiTile label="Ersparnis Prompt-Cache" icon="database" value={amount(s.savings.providerCache)} unit={cur} hint="{short(s.savings.providerCacheTokens)} Tokens aus dem Anbieter-Cache" testid="kpi-provider-cache" />
      <KpiTile label="Plugin-Kosten" icon="puzzle" value={amount(s.plugins.amount)} unit={cur} hint="{percent(s.plugins.share ?? 0)} der Kosten" testid="kpi-plugins" />
    </div>

    <Card class="mt-4" title="Kostenverlauf nach {dimLabel(groupBy)}" description="Gestapelt, Top 5 und Übrige · {cur}" testid="cost-chart">
      {#snippet actions()}
        <div class="flex flex-wrap items-center gap-2">
          <Select label="Gruppieren nach" items={DIMS.map((d) => ({ value: d.value, label: d.label }))} value={groupBy} onchange={(v) => (groupBy = v as CostDim)} size="sm" class="w-44" />
          <ToggleGroup.Root type="single" value={step || 'auto'} onValueChange={(x) => x && (step = x === 'auto' ? '' : x)} class={segment} aria-label="Schrittweite">
            <ToggleGroup.Item value="auto" class={segmentItem}>Auto</ToggleGroup.Item>
            <ToggleGroup.Item value="1h" class={segmentItem} disabled={range === '90d'}>Stunde</ToggleGroup.Item>
            <ToggleGroup.Item value="1d" class={segmentItem}>Tag</ToggleGroup.Item>
          </ToggleGroup.Root>
        </div>
      {/snippet}
      <AsyncState loader={series} rows={6}>
        {#snippet children(ts)}
          {@const t = (ts.series[0]?.points ?? []).map((p) => Date.parse(p.t) / 1000)}
          {@const rows = ts.series.map((x) => ({ key: x.key, label: groupName(groupBy, x.key, x.name), values: x.points.map((p) => p.v ?? 0), total: x.totalAmount ?? String(x.total ?? 0) }))}
          {#if rows.every((r) => r.values.every((v) => v === 0))}
            <EmptyState compact icon="coins" title="Keine Kosten im Zeitraum" text="Kosten entstehen mit Preisen für die genutzten Anbieter-Modelle (Seite Preise)." />
          {:else}
            {#await stacked()}
              <div class="h-56 animate-pulse rounded-md bg-skeleton"></div>
            {:then mod}
              {@const Stacked = mod.default}
              <Stacked {t} series={rows} format={(v) => amount(v)} label="Kosten nach {dimLabel(groupBy)} im Zeitverlauf (je {ts.step})" height="h-64" />
            {/await}
          {/if}
          <details class="mt-3 text-sm" data-testid="cost-chart-table">
            <summary class="cursor-pointer text-fg-muted hover:text-fg">Als Tabelle anzeigen</summary>
            <div class="scrollbar-thin mt-2 max-h-80 overflow-auto">
              <table class="w-full text-left">
                <caption class="sr-only">Kosten je {ts.step} nach {dimLabel(groupBy)}</caption>
                <thead>
                  <tr class="border-b border-line text-fg-muted">
                    <th class="px-2 py-1 font-medium">Zeit</th>
                    {#each rows as r (r.key)}<th class="px-2 py-1 text-right font-medium">{r.label}</th>{/each}
                  </tr>
                </thead>
                <tbody>
                  {#each t as x, i (x)}
                    <tr class="border-b border-line last:border-0">
                      <td class="px-2 py-1 whitespace-nowrap">{dateTime(x * 1000)}</td>
                      {#each rows as r (r.key)}<td class="num px-2 py-1 text-right">{amount(r.values[i])}</td>{/each}
                    </tr>
                  {/each}
                  <tr class="font-medium">
                    <td class="px-2 py-1">Summe</td>
                    {#each rows as r (r.key)}<td class="num px-2 py-1 text-right">{amount(r.total)}</td>{/each}
                  </tr>
                </tbody>
              </table>
            </div>
          </details>
        {/snippet}
      </AsyncState>
    </Card>

    <div class="mt-4 grid gap-4 xl:grid-cols-5">
      <Card title="Aufschlüsselung" description="Anteile an den Gesamtkosten" class="xl:col-span-3" testid="cost-breakdown">
        <ul class="flex flex-col gap-3">
          {#each s.breakdown as p (p.part)}
            <li class="grid grid-cols-[1fr_auto] items-center gap-x-3 gap-y-1">
              <span class="text-base font-medium text-fg">{PART_LABEL[p.part] ?? p.part}</span>
              <span class="num text-sm text-fg-muted">{amount(p.amount)} {cur} · {percent(p.share ?? 0)}</span>
              <Meter value={p.share ?? 0} label="Anteil {PART_LABEL[p.part] ?? p.part}" tone="chart" class="col-span-2" />
            </li>
          {/each}
        </ul>
        <p class="mt-3 text-xs text-fg-subtle">
          Plugins: Prüfmodelle von content-safety und Wiederholungen von schema-guard ({short(s.plugins.inputTokens)} Eingabe-, {short(s.plugins.outputTokens)} Ausgabe-Tokens). „Sonstiges“: Preise je Anfrage, Bild, Audio oder Zeichen sowie Daten vor der Aufschlüsselung.
        </p>
      </Card>
      <Card title="Ersparnis" description="durch Caching im Zeitraum" class="xl:col-span-2" testid="cost-savings">
        <dl class="grid grid-cols-[1fr_auto] gap-x-4 gap-y-3 text-base">
          <dt>
            Prompt-Caching des Anbieters
            <span class="block text-xs text-fg-subtle">exakt: Cache-Tokens × (Eingabe- − Cache-Lesepreis)</span>
          </dt>
          <dd class="num text-right">{amount(s.savings.providerCache)} {cur}</dd>
          <dt>
            Antwort-Cache des Gateways <Badge tone="warning">Schätzung</Badge>
            <span class="block text-xs text-fg-subtle">{short(s.savings.gatewayCacheHits)} Treffer × mittlere Kosten einer bezahlten Anfrage der Modellgruppe</span>
          </dt>
          <dd class="num text-right" data-testid="gateway-cache-estimate">{s.savings.gatewayCacheEstimate == null ? '–' : `≈ ${amount(s.savings.gatewayCacheEstimate)} ${cur}`}</dd>
        </dl>
        {#if s.savings.gatewayCacheHitsUnestimated > 0}
          <p class="mt-3 text-xs text-fg-subtle">{short(s.savings.gatewayCacheHitsUnestimated)} Treffer ohne Vergleichsbasis (keine bezahlte Anfrage der Modellgruppe im Zeitraum) sind nicht eingerechnet.</p>
        {/if}
      </Card>
    </div>

    <Card class="mt-4" title="Kostentreiber nach {dimLabel(groupBy)}" description="Vergleich mit der Vorperiode ab {day(Date.parse(drivers.previousFrom))}; Klick filtert die Seite" flush testid="cost-drivers">
      {#if drivers.items.length === 0}
        <EmptyState compact icon="coins" title="Keine Kosten im Zeitraum" />
      {:else}
        <div class="scrollbar-thin overflow-x-auto">
          <table class="w-full text-left text-base">
            <caption class="sr-only">Kostentreiber nach {dimLabel(groupBy)} mit Vorperiode</caption>
            <thead>
              <tr class="border-b border-line text-sm text-fg-muted">
                <th class="px-4 py-2 font-medium">{dimLabel(groupBy)}</th>
                <th class="px-4 py-2 text-right font-medium">Kosten ({cur})</th>
                <th class="px-4 py-2 text-right font-medium">Vorperiode</th>
                <th class="px-4 py-2 text-right font-medium">Δ</th>
                <th class="px-4 py-2 text-right font-medium">Δ %</th>
                <th class="px-4 py-2 text-right font-medium">Anteil</th>
                <th class="hidden px-4 py-2 text-right font-medium md:table-cell">Anfragen</th>
                <th class="hidden px-4 py-2 text-right font-medium md:table-cell">je Anfrage</th>
                <th class="px-4 py-2"><span class="sr-only">Aktionen</span></th>
              </tr>
            </thead>
            <tbody>
              {#each drivers.items as it (it.key)}
                {@const label = groupName(groupBy, it.key, it.name)}
                {@const logs = logsLink(groupBy, it)}
                <tr class="border-b border-line last:border-0" data-testid="driver-row">
                  <td class="px-4 py-2.5">
                    {#if it.key}
                      <button type="button" class="text-left font-mono text-sm text-fg hover:text-accent-text" onclick={() => drill(groupBy, it)} aria-label="Nach {label} filtern">{label}</button>
                    {:else}
                      <span class="text-sm text-fg-muted">{label}</span>
                    {/if}
                  </td>
                  <td class="num px-4 py-2.5 text-right">{amount(it.cost.amount)}</td>
                  <td class="num px-4 py-2.5 text-right text-fg-muted">{amount(it.cost.previous)}</td>
                  <td class={['num px-4 py-2.5 text-right', n(it.cost.delta) > 0 ? 'text-danger-fg' : n(it.cost.delta) < 0 ? 'text-success-fg' : 'text-fg-muted']}>{signed(it.cost.delta)}</td>
                  <td class="num px-4 py-2.5 text-right text-fg-muted">{it.cost.deltaPercent == null ? 'neu' : `${it.cost.deltaPercent > 0 ? '+' : ''}${it.cost.deltaPercent.toLocaleString('de-DE', { maximumFractionDigits: 1 })} %`}</td>
                  <td class="num px-4 py-2.5 text-right">{percent(it.share ?? 0)}</td>
                  <td class="num hidden px-4 py-2.5 text-right md:table-cell">{short(it.requests)}</td>
                  <td class="num hidden px-4 py-2.5 text-right md:table-cell">{it.costPerRequest ? amount(it.costPerRequest, 4) : '–'}</td>
                  <td class="px-4 py-2.5 text-right">
                    {#if logs}<a class="text-sm whitespace-nowrap text-fg-muted hover:text-accent-text" href={logs}>Logs</a>{/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </Card>
  {/snippet}
</AsyncState>

<div class="mt-4 grid gap-4 xl:grid-cols-5">
  <Card title="Budgets" description="Verbrauch im laufenden Zeitraum und Prognose bis Periodenende (Durchschnitt der letzten 7 Tage)" class="xl:col-span-3" testid="cost-budgets">
    <AsyncState loader={budgets} rows={4}>
      {#snippet children(b)}
        {#if b.items.length === 0}
          <EmptyState compact icon="gauge" title="Keine Budgets" text="Budgets lassen sich an Organisationen, Teams und Keys festlegen." />
        {:else}
          <ul class="flex flex-col divide-y divide-line">
            {#each b.items as it (it.budget.level + it.budget.targetId)}
              {@const h = HEALTH[it.health]}
              <li class="flex flex-col gap-1.5 py-3 first:pt-0 last:pb-0" data-testid="budget-row">
                <div class="flex flex-wrap items-center gap-2">
                  <button
                    type="button"
                    class="font-mono text-sm text-fg hover:text-accent-text"
                    aria-pressed={selected?.target === it.budget.targetName && selected?.level === it.budget.level}
                    onclick={() => (selected = { level: it.budget.level, target: it.budget.targetName })}>{it.budget.targetName}</button
                  >
                  <span class="text-xs text-fg-muted">{LEVEL[it.budget.level] ?? it.budget.level} · {PERIOD[it.budget.period] ?? it.budget.period}</span>
                  <Badge tone={h?.tone ?? 'neutral'} dot>{h?.label ?? it.health}</Badge>
                  <span class="num ml-auto text-sm">{amount(it.budget.used)} / {amount(it.budget.amount)} {b.currency}</span>
                </div>
                <Meter value={it.budget.percent / 100} label="Verbrauch {it.budget.targetName}" tone="auto" />
                <p class="text-xs text-fg-muted">
                  <span class="font-medium">Prognose</span>:
                  {#if it.forecast.projected != null}≈ {amount(it.forecast.projected)} {b.currency} ({it.forecast.projectedPercent} %) bis {it.budget.periodEnd ? day(Date.parse(it.budget.periodEnd)) : 'Periodenende'}{:else}kein Periodenende{/if}
                  · {amount(it.forecast.dailyRate)} {b.currency}/Tag
                  {#if it.forecast.exceedsAt}· <span class="text-danger-fg">überschritten voraussichtlich am {day(Date.parse(it.forecast.exceedsAt))}</span>{/if}
                </p>
              </li>
            {/each}
          </ul>
        {/if}
      {/snippet}
    </AsyncState>
  </Card>
  <Card title="Burn-down" description={selected ? `${selected.target}: kumulierte Kosten und Prognose` : 'Budget links auswählen'} class="xl:col-span-2" testid="cost-burndown">
    {#if !selected}
      <EmptyState compact icon="trend-up" title="Kein Budget gewählt" text="Ein Budget in der Liste anklicken, um Verbrauch und Prognose im Zeitverlauf zu sehen." />
    {:else}
      <AsyncState loader={burn} rows={4}>
        {#snippet children(f)}
          {#if !f || f.burnDown.length === 0}
            <EmptyState compact icon="trend-up" title="Kein Verlauf" text="Für Budgets ohne Periodenende (gesamt) gibt es keinen Burn-down." />
          {:else}
            {@const t = f.burnDown.map((p) => Date.parse(p.t) / 1000)}
            {@const last = f.burnDown.reduce((acc, p, i) => (p.spent != null ? i : acc), -1)}
            <TimeSeriesChart
              {t}
              series={[
                { label: 'Verbraucht', values: f.burnDown.map((p) => (p.spent != null ? n(p.spent) : null) as number), color: '--color-chart-1' },
                { label: 'Prognose', values: f.burnDown.map((p, i) => (p.projected != null ? n(p.projected) : i === last ? n(p.spent) : null) as number), color: '--color-chart-2' },
                { label: 'Budget', values: f.burnDown.map(() => n(f.budget.amount)), color: '--color-chart-4' },
              ]}
              format={(v) => amount(v)}
              label="Burn-down {f.budget.targetName}"
              height={200}
              area={false}
            />
            <p class="mt-2 text-xs text-fg-subtle">Prognose = Hochrechnung, keine Zusage. Verlauf aus den Rollups; „Verbraucht“ der Liste aus dem Budget-Ledger.</p>
          {/if}
        {/snippet}
      </AsyncState>
    {/if}
  </Card>
</div>

<Card class="mt-4" title="Chargeback-Export" description="Kosten je Abschnitt und Gruppe im gewählten Zeitraum, mit Kostenstelle, Labels und Währung (gestreamt)" testid="cost-export">
  <div class="flex flex-wrap items-end gap-3">
    <div class="flex flex-col gap-1">
      <span class="text-sm text-fg-muted" aria-hidden="true">Gruppe</span>
      <Select label="Gruppe" items={DIMS.map((d) => ({ value: d.value, label: d.label }))} value={expDim} onchange={(v) => (expDim = v as CostDim)} class="w-44" />
    </div>
    <div class="flex flex-col gap-1">
      <span class="text-sm text-fg-muted" aria-hidden="true">Abschnitte</span>
      <Select
        label="Abschnitte"
        items={[
          { value: 'total', label: 'Gesamt' },
          { value: 'month', label: 'Monat' },
          { value: 'week', label: 'Woche' },
          { value: 'day', label: 'Tag' },
        ]}
        value={expGran}
        onchange={(v) => (expGran = v as Granularity)}
        class="w-36"
      />
    </div>
    <ToggleGroup.Root type="single" value={expFormat} onValueChange={(x) => x && (expFormat = x as ExportFormat)} class={segment} aria-label="Format">
      <ToggleGroup.Item value="csv" class={segmentItem}>CSV</ToggleGroup.Item>
      <ToggleGroup.Item value="json" class={segmentItem}>JSON</ToggleGroup.Item>
    </ToggleGroup.Root>
    <Button icon="download" onclick={runExport} loading={exporting} data-testid="cost-export-button">Herunterladen</Button>
  </div>
</Card>
