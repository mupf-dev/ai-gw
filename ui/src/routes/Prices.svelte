<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Preise (AIGW-49): Versionen je (Anbieter, Modell) mit Zustand „gültig“, „abgelöst“, „künftig“. Gültige und
  // abgelöste Versionen sind unveränderlich (Server: 409) – neue Version anlegen; künftige dürfen geändert/gelöscht werden.
  // Darunter Preisvorschläge (AIGW-85) aus Azure Retail Prices und Preiskatalog.
  import { prices, PROVIDERS, providerLabel, type Price } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf, Loader } from '../lib/admin/errors.svelte';
  import { dateTime } from '../lib/format';
  import { canWrite } from '../lib/rbac.svelte';
  import { toast } from '../lib/toast.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../lib/ui/DataTable.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Select from '../lib/ui/Select.svelte';
  import PriceFormDialog from './prices/PriceFormDialog.svelte';
  import PriceSuggestions from './prices/PriceSuggestions.svelte';

  const data = new Loader(() => prices.list());
  let provider = $state('');
  let phase = $state('all');
  let q = $state('');
  let formOpen = $state(false);
  let editName = $state<string | null>(null);
  let template = $state<Price | null>(null);
  let del = $state<string | null>(null);
  let delOpen = $state(false);

  const writable = $derived(canWrite({ kind: 'catalog' }));
  const now = Date.now();
  const key = (p: Price) => `${p.spec.provider ?? 'any'}|${p.spec.model}`;
  /** Zustand je Version: künftig, gültig (jüngste mit validFrom ≤ jetzt), abgelöst. */
  const status = $derived.by(() => {
    const m = new Map<string, 'future' | 'current' | 'past'>();
    const byKey = new Map<string, Price[]>();
    for (const p of data.data ?? []) byKey.set(key(p), [...(byKey.get(key(p)) ?? []), p]);
    for (const list of byKey.values()) {
      const past = list.filter((p) => Date.parse(p.spec.validFrom) <= now).sort((a, b) => Date.parse(b.spec.validFrom) - Date.parse(a.spec.validFrom));
      for (const p of list) m.set(p.metadata.name, Date.parse(p.spec.validFrom) > now ? 'future' : p === past[0] ? 'current' : 'past');
    }
    return m;
  });
  const rows = $derived(
    (data.data ?? [])
      .filter((p) => !provider || (provider === 'any' ? !p.spec.provider : p.spec.provider === provider))
      .filter((p) => phase === 'all' || status.get(p.metadata.name) === phase),
  );

  function create(t: Price | null) {
    editName = null;
    template = t;
    formOpen = true;
  }
  function edit(p: Price) {
    editName = p.metadata.name;
    template = null;
    formOpen = true;
  }
  async function runDelete(): Promise<string | null> {
    if (!del) return null;
    try {
      const { etag } = await prices.get(del);
      await prices.remove(del, etag);
      toast('success', 'Preisversion gelöscht', del);
      void data.reload();
      return null;
    } catch (e) {
      return formErrorOf(e, 'Preisversion löschen', () => void data.reload());
    }
  }
  const tp = (p: Price, k: 'input' | 'cachedInput' | 'output') => p.spec.perMillionTokens?.[k] ?? '–';
  const others = (p: Price) => {
    const t = p.spec.perMillionTokens ?? {};
    const n = Object.entries(t).filter(([k, v]) => v != null && !['input', 'cachedInput', 'output'].includes(k)).length;
    return n + (p.spec.perRequest ? 1 : 0) + (p.spec.perImage ? 1 : 0) + (p.spec.perAudioMinute ? 1 : 0) + (p.spec.perMillionCharacters ? 1 : 0);
  };

  const columns: Column<Price>[] = [
    { id: 'model', label: 'Modell', cell: modelCell, sort: (p) => `${p.spec.model} ${p.spec.validFrom}` },
    { id: 'from', label: 'Gültig ab', cell: fromCell, sort: (p) => Date.parse(p.spec.validFrom), from: 'sm' },
    { id: 'input', label: 'Eingabe', cell: inCell, align: 'right', from: 'md' },
    { id: 'cached', label: 'Cache', cell: cachedCell, align: 'right', from: 'lg' },
    { id: 'output', label: 'Ausgabe', cell: outCell, align: 'right', from: 'md' },
    { id: 'state', label: 'Zustand', cell: stateCell, sort: (p) => status.get(p.metadata.name) ?? '' },
    { id: 'actions', label: 'Aktionen', cell: actionsCell, srOnlyLabel: true, align: 'right', class: 'w-48' },
  ];
</script>

{#snippet modelCell(p: Price)}
  <span class="block font-mono text-sm font-medium text-fg">{p.spec.model}</span>
  <span class="block text-xs text-fg-muted">{providerLabel(p.spec.provider)}{others(p) ? ` · +${others(p)} weitere Preise` : ''}</span>
{/snippet}
{#snippet fromCell(p: Price)}<span class="num text-sm text-fg-muted">{dateTime(Date.parse(p.spec.validFrom))}</span>{/snippet}
{#snippet inCell(p: Price)}<span class="num text-sm">{tp(p, 'input')}</span>{/snippet}
{#snippet cachedCell(p: Price)}<span class="num text-sm text-fg-muted">{tp(p, 'cachedInput')}</span>{/snippet}
{#snippet outCell(p: Price)}<span class="num text-sm">{tp(p, 'output')}</span>{/snippet}
{#snippet stateCell(p: Price)}
  {@const s = status.get(p.metadata.name)}
  {#if s === 'current'}<Badge tone="success" dot>gültig</Badge>{:else if s === 'future'}<Badge tone="info" dot>künftig</Badge>{:else}<Badge tone="neutral" dot>abgelöst</Badge>{/if}
{/snippet}
{#snippet actionsCell(p: Price)}
  {#if writable}
    <span class="inline-flex gap-1">
      {#if status.get(p.metadata.name) === 'future'}
        <Button size="sm" variant="ghost" icon="pencil" onclick={() => edit(p)}>Ändern</Button>
        <Button
          size="sm"
          variant="ghost"
          icon="trash"
          aria-label="{p.metadata.name} löschen"
          onclick={() => {
            del = p.metadata.name;
            delOpen = true;
          }}
        />
      {:else}
        <Button size="sm" variant="ghost" icon="plus" onclick={() => create(p)}>Neue Version</Button>
      {/if}
    </span>
  {/if}
{/snippet}

<PageHeader title="Preise" description="Versionierte Preise je Anbieter-Modell. Es gilt die jüngste Version mit „gültig ab“ ≤ Anfragezeitpunkt; vergangene Versionen bleiben für die Abrechnung unverändert.">
  {#snippet actions()}
    {#if writable}<Button variant="primary" icon="plus" onclick={() => create(null)} data-testid="create-price">Preisversion anlegen</Button>{/if}
  {/snippet}
</PageHeader>

  <Card flush testid="prices-card">
    <AsyncState loader={data}>
      {#snippet children()}
        <DataTable
          {rows}
          {columns}
          bind:q
          key={(p) => p.metadata.name}
          caption="Preisversionen"
          search={(p) => `${p.metadata.name} ${p.spec.model} ${p.spec.provider ?? ''}`}
          searchPlaceholder="Modell oder Anbieter …"
          sortId="model"
          perPage={25}
          testid="prices-table"
          resetFilters={() => {
            provider = '';
            phase = 'all';
          }}
        >
          {#snippet toolbar()}
            <div class="w-44">
              <Select size="sm" label="Anbieter" bind:value={provider} items={[{ value: '', label: 'Alle Anbieter' }, { value: 'any', label: 'ohne Anbieter' }, ...PROVIDERS.map((x) => ({ value: x.value, label: x.label }))]} />
            </div>
            <div class="w-36">
              <Select
                size="sm"
                label="Zustand"
                bind:value={phase}
                items={[
                  { value: 'all', label: 'Alle Versionen' },
                  { value: 'current', label: 'gültig' },
                  { value: 'future', label: 'künftig' },
                  { value: 'past', label: 'abgelöst' },
                ]}
              />
            </div>
          {/snippet}
          {#snippet empty()}
            <EmptyState icon="coins" title="Noch keine Preise" text="Ohne Preis werden Anfragen mit Kosten 0 protokolliert.">
              {#if writable}<Button variant="primary" icon="plus" onclick={() => create(null)}>Preisversion anlegen</Button>{/if}
            </EmptyState>
          {/snippet}
        </DataTable>
      {/snippet}
    </AsyncState>
  </Card>

<div class="mt-6">
  <PriceSuggestions {writable} onaccepted={() => void data.reload()} />
</div>

<PriceFormDialog
  bind:open={formOpen}
  {editName}
  {template}
  onsaved={(n) => {
    toast('success', 'Preisversion gespeichert', n);
    void data.reload();
  }}
/>
<ConfirmDialog bind:open={delOpen} title="Künftige Preisversion löschen?" confirmLabel="Löschen" onconfirm={runDelete}>
  <p>„<span class="font-mono text-fg">{del}</span>“ wird gelöscht; bis dahin gilt weiter die vorige Version.</p>
</ConfirmDialog>
