<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Preisvorschläge (AIGW-85): Vorschlag (Azure Retail Prices bzw. Preiskatalog) neben dem aktuell gültigen Preis je Deployment,
  // Auswahl + Übernahme als neue Preisversionen. Manuelle Preise haben Vorrang – abweichende nur mit ausdrücklicher Bestätigung.
  import { priceSuggestions, providerLabel, type PriceSuggestion } from '../../lib/api/resources';
  import AsyncState from '../../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf, Loader } from '../../lib/admin/errors.svelte';
  import { dateTime } from '../../lib/format';
  import { toast } from '../../lib/toast.svelte';
  import Badge, { type Tone } from '../../lib/ui/Badge.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../../lib/ui/DataTable.svelte';
  import Icon from '../../lib/ui/Icon.svelte';

  let { writable, onaccepted }: { writable: boolean; onaccepted: () => void } = $props();

  let refresh = false;
  const data = new Loader(() => priceSuggestions.list(refresh));
  let q = $state('');
  let selected = $state<Record<string, boolean>>({});
  let confirmOpen = $state(false);

  const items = $derived(data.data?.items ?? []);
  const unpriced = $derived(items.filter((i) => !i.current));
  const selectable = (i: PriceSuggestion) => !!i.suggested && i.status !== 'unchanged';
  const chosen = $derived(items.filter((i) => selected[i.deployment] && selectable(i)));
  const manualChosen = $derived(chosen.filter((i) => i.current?.manual));

  const STATUS: Record<PriceSuggestion['status'], { label: string; tone: Tone }> = {
    missing: { label: 'kein Preis', tone: 'warning' },
    unchanged: { label: 'aktuell', tone: 'success' },
    differs: { label: 'abweichend', tone: 'info' },
    conflict: { label: 'Konflikt', tone: 'danger' },
    'no-suggestion': { label: 'kein Vorschlag', tone: 'neutral' },
  };
  const SOURCE: Record<string, string> = { 'azure-retail': 'Azure Retail Prices', catalog: 'Preiskatalog' };

  /** Vorauswahl: fehlende Preise und abweichende nicht-manuelle Preise (Konflikte nie automatisch). */
  $effect(() => {
    const next: Record<string, boolean> = {};
    for (const i of data.data?.items ?? []) next[i.deployment] = selectable(i) && (i.status === 'missing' || (i.status === 'differs' && !i.current?.manual));
    selected = next;
  });

  async function reload(force: boolean) {
    refresh = force;
    await data.reload();
    refresh = false;
  }

  async function accept(): Promise<string | null> {
    try {
      const r = await priceSuggestions.accept({ items: chosen.map((i) => ({ deployment: i.deployment, overrideManual: !!i.current?.manual })) });
      toast('success', `${r.created.length} Preisversion(en) angelegt`, r.skipped.length ? `${r.skipped.length} übersprungen` : undefined);
      onaccepted();
      void data.reload();
      return null;
    } catch (e) {
      return formErrorOf(e, 'Preisvorschläge übernehmen', () => void data.reload());
    }
  }

  const tp = (p: { perMillionTokens?: { input?: string | null; cachedInput?: string | null; output?: string | null } } | undefined | null) =>
    p ? `${p.perMillionTokens?.input ?? '–'} / ${p.perMillionTokens?.cachedInput ?? '–'} / ${p.perMillionTokens?.output ?? '–'}` : '–';

  const columns: Column<PriceSuggestion>[] = [
    { id: 'sel', label: 'Auswahl', cell: selCell, srOnlyLabel: true, class: 'w-10' },
    { id: 'deployment', label: 'Deployment', cell: depCell, sort: (i) => i.deployment },
    { id: 'source', label: 'Quelle', cell: sourceCell, sort: (i) => i.source ?? '', from: 'md' },
    { id: 'suggested', label: 'Vorschlag (Ein / Cache / Aus)', cell: suggestedCell, align: 'right' },
    { id: 'current', label: 'Aktuell', cell: currentCell, align: 'right', from: 'sm' },
    { id: 'status', label: 'Status', cell: statusCell, sort: (i) => i.status },
  ];
</script>

{#snippet selCell(i: PriceSuggestion)}
  {#if writable && selectable(i)}
    <input type="checkbox" class="accent-accent" aria-label="Vorschlag für {i.deployment} übernehmen" bind:checked={selected[i.deployment]} data-testid="suggest-select-{i.deployment}" />
  {/if}
{/snippet}
{#snippet depCell(i: PriceSuggestion)}
  <span class="block font-mono text-sm font-medium text-fg">{i.deployment}</span>
  <span class="block text-xs text-fg-muted">{i.modelGroup} · {providerLabel(i.provider)} · <span class="font-mono">{i.providerModel}</span></span>
  {#each i.notes ?? [] as n (n)}<span class="block text-xs text-fg-muted">{n}</span>{/each}
{/snippet}
{#snippet sourceCell(i: PriceSuggestion)}
  {#if i.source}
    <span class="block text-sm">{SOURCE[i.source] ?? i.source}</span>
    <span class="block text-xs text-fg-muted">{[i.region, i.deploymentType, i.matched].filter(Boolean).join(' · ')}</span>
    {#if i.meters?.length}<span class="block font-mono text-xs text-fg-muted" title={i.meters.join('\n')}>{i.meters.length} Meter</span>{/if}
  {:else}<span class="text-sm text-fg-muted">–</span>{/if}
{/snippet}
{#snippet suggestedCell(i: PriceSuggestion)}
  <span class="num block text-sm">{tp(i.suggested)}</span>
  {#if i.suggested}<span class="block text-xs text-fg-muted">ab {dateTime(Date.parse(i.suggested.validFrom))}</span>{/if}
{/snippet}
{#snippet currentCell(i: PriceSuggestion)}
  <span class="num block text-sm text-fg-muted">{tp(i.current?.spec)}</span>
  {#if i.current}
    <span class="block text-xs text-fg-muted">{i.current.manual ? 'manuell' : 'aus Vorschlag'} · <span class="font-mono">{i.current.name}</span></span>
  {/if}
{/snippet}
{#snippet statusCell(i: PriceSuggestion)}
  <Badge tone={STATUS[i.status].tone} dot>{STATUS[i.status].label}</Badge>
  {#if i.status === 'differs' && i.current?.manual}<span class="mt-1 block text-xs text-fg-muted">manuell hat Vorrang</span>{/if}
{/snippet}

<Card flush testid="price-suggestions-card">
  <div class="flex flex-wrap items-center justify-between gap-3 border-b border-border px-4 py-3">
    <div>
      <h2 class="flex items-center gap-2 text-base font-semibold text-fg"><Icon name="sparkles" /> Preisvorschläge</h2>
      <p class="text-sm text-fg-muted">Aus der Azure Retail Prices API (Azure OpenAI, Foundry) und dem Preiskatalog der Anbieter, in USD je 1 Mio. Tokens.</p>
    </div>
    <span class="inline-flex gap-2">
      <Button size="sm" variant="ghost" icon="refresh" onclick={() => reload(true)} data-testid="suggest-refresh">Neu abrufen</Button>
      {#if writable}
        <Button size="sm" variant="primary" icon="check" disabled={chosen.length === 0} onclick={() => (confirmOpen = true)} data-testid="suggest-accept">
          Übernehmen ({chosen.length})
        </Button>
      {/if}
    </span>
  </div>
  <AsyncState loader={data}>
    {#snippet children()}
      {#if unpriced.length}
        <div class="flex items-start gap-2 border-b border-warning-fg/30 bg-warning-bg px-4 py-2 text-sm text-warning-fg" role="status" data-testid="unpriced-hint">
          <Icon name="alert" class="mt-0.5 shrink-0" />
          <span>{unpriced.length} Deployment(s) ohne Preis – Anfragen werden mit Kosten 0 gebucht: <span class="font-mono">{unpriced.map((i) => i.deployment).join(', ')}</span></span>
        </div>
      {/if}
      {#each (data.data?.sources ?? []).filter((s) => s.error) as s (s.source + s.scope)}
        <div class="border-b border-border px-4 py-2 text-sm text-fg-muted" role="status">
          {SOURCE[s.source]} ({s.scope}): {s.error}{s.fetchedAt ? ` – Daten vom ${dateTime(Date.parse(s.fetchedAt))}` : ''}
        </div>
      {/each}
      <DataTable
        rows={items}
        {columns}
        bind:q
        key={(i) => i.deployment}
        caption="Preisvorschläge je Deployment"
        search={(i) => `${i.deployment} ${i.modelGroup} ${i.providerModel} ${i.provider}`}
        searchPlaceholder="Deployment oder Modell …"
        sortId="deployment"
        perPage={25}
        testid="price-suggestions-table"
      />
    {/snippet}
  </AsyncState>
</Card>

<ConfirmDialog bind:open={confirmOpen} title="Preisvorschläge übernehmen?" confirmLabel="Übernehmen" icon="check" danger={manualChosen.length > 0} onconfirm={accept} testid="suggest-confirm">
  <p>{chosen.length} neue Preisversion(en) gelten ab jetzt; bisherige Versionen bleiben für die Abrechnung erhalten.</p>
  {#if manualChosen.length}
    <p class="mt-2">Ersetzt manuell gepflegte Preise: <span class="font-mono text-fg">{manualChosen.map((i) => i.deployment).join(', ')}</span></p>
  {/if}
</ConfirmDialog>
