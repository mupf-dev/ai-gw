<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Keys: Liste mit Suche/Filtern/Sortierung/Pagination, Zeilenaktionen, „Key erstellen“ (einmalige Anzeige).
  import { DropdownMenu } from 'bits-ui';
  import { ctx, currentOrg } from '../lib/context.svelte';
  import { keys, revokeKey } from '../lib/data.svelte';
  import { ago, day, money, short } from '../lib/format';
  import { teamName } from '../lib/mock/data';
  import type { ApiKey } from '../lib/mock/types';
  import { route } from '../lib/router.svelte';
  import { toast } from '../lib/toast.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../lib/ui/DataTable.svelte';
  import Dialog from '../lib/ui/Dialog.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import Meter from '../lib/ui/Meter.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Select from '../lib/ui/Select.svelte';
  import { btn, cx, menuItem, menuSeparator, popover } from '../lib/ui/styles';
  import CreateKeyDialog from './keys/CreateKeyDialog.svelte';

  const params = new URLSearchParams(route.search);
  let createOpen = $state(params.get('new') === '1');
  let q = $state(params.get('q') ?? '');
  let status = $state('all');
  let revoking = $state<ApiKey | null>(null);
  let revokeOpen = $state(false);

  const res = $derived(keys());
  const all = $derived(res.state === 'ok' ? res.data : []);
  const rows = $derived(all.filter((k) => (status === 'all' || k.status === status) && (!ctx.teamId || k.teamId === ctx.teamId)));
  const DAY = 86_400_000;
  const expiringSoon = (k: ApiKey) => k.status === 'active' && k.expiresAt !== null && k.expiresAt - Date.now() < 14 * DAY;

  function askRevoke(k: ApiKey) {
    revoking = k;
    revokeOpen = true;
  }
  function confirmRevoke() {
    if (!revoking) return;
    revokeKey(revoking.id);
    toast('success', 'Key gesperrt', `„${revoking.name}“ wird ab sofort abgewiesen.`);
    revokeOpen = false;
  }

  const columns: Column<ApiKey>[] = [
    { id: 'name', label: 'Name', cell: nameCell, sort: (k) => k.name },
    { id: 'team', label: 'Team', cell: teamCell, sort: (k) => teamName(k.teamId), from: 'md' },
    { id: 'models', label: 'Modelle', cell: modelsCell, from: 'xl' },
    { id: 'budget', label: 'Budget / Monat', cell: budgetCell, sort: (k) => (k.budgetLimit ? k.budgetUsed / k.budgetLimit : -1), from: 'lg', class: 'w-52' },
    { id: 'limits', label: 'Limits', cell: limitsCell, from: 'xl' },
    { id: 'used', label: 'Zuletzt genutzt', cell: usedCell, sort: (k) => k.lastUsedAt ?? 0, from: 'sm' },
    { id: 'status', label: 'Status', cell: statusCell, sort: (k) => k.status },
    { id: 'actions', label: 'Aktionen', cell: actionsCell, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
</script>

{#snippet nameCell(k: ApiKey)}
  <div class="flex min-w-0 flex-col">
    <span class="truncate font-medium text-fg">{k.name}</span>
    <span class="font-mono text-xs text-fg-muted">{k.prefix}…{k.last4}</span>
  </div>
{/snippet}
{#snippet teamCell(k: ApiKey)}<span class="text-fg-muted">{teamName(k.teamId)}</span>{/snippet}
{#snippet modelsCell(k: ApiKey)}
  {#if k.models === 'all'}
    <Badge tone="outline">Alle Modelle</Badge>
  {:else}
    <span class="flex flex-wrap gap-1">
      {#each k.models.slice(0, 2) as m (m)}<Badge mono>{m}</Badge>{/each}
      {#if k.models.length > 2}<Badge tone="outline">+{k.models.length - 2}</Badge>{/if}
    </span>
  {/if}
{/snippet}
{#snippet budgetCell(k: ApiKey)}
  {#if k.budgetLimit}
    <div class="flex flex-col gap-1">
      <span class="num text-sm whitespace-nowrap"><span class="text-fg">{money(k.budgetUsed)}</span><span class="text-fg-subtle"> / {money(k.budgetLimit)}</span></span>
      <Meter value={k.budgetUsed / k.budgetLimit} tone="auto" label="Budget {k.name}" />
    </div>
  {:else}
    <span class="text-sm text-fg-muted">unbegrenzt · <span class="num">{money(k.budgetUsed)}</span></span>
  {/if}
{/snippet}
{#snippet limitsCell(k: ApiKey)}
  <span class="num flex flex-col text-xs whitespace-nowrap text-fg-muted">
    <span>{k.rpm ? `${short(k.rpm)} RPM` : 'RPM unbegrenzt'}</span>
    <span>{k.tpm ? `${short(k.tpm)} TPM` : 'TPM unbegrenzt'}</span>
  </span>
{/snippet}
{#snippet usedCell(k: ApiKey)}<span class="text-sm text-fg-muted">{k.lastUsedAt ? ago(k.lastUsedAt) : 'nie'}</span>{/snippet}
{#snippet statusCell(k: ApiKey)}
  {#if k.status === 'revoked'}<Badge tone="danger" dot>Gesperrt</Badge>
  {:else if k.status === 'expired'}<Badge tone="neutral" dot>Abgelaufen</Badge>
  {:else if expiringSoon(k)}<Badge tone="warning" dot>Läuft ab {day(k.expiresAt ?? 0)}</Badge>
  {:else}<Badge tone="success" dot>Aktiv</Badge>{/if}
{/snippet}
{#snippet actionsCell(k: ApiKey)}
  <DropdownMenu.Root>
    <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {k.name}"><Icon name="more" /></DropdownMenu.Trigger>
    <DropdownMenu.Portal>
      <DropdownMenu.Content class={cx(popover, 'w-48')} align="end" sideOffset={4}>
        <DropdownMenu.Item class={menuItem} onSelect={() => toast('info', 'Bearbeiten', 'Detailseite folgt mit AIGW-Keys.')}><Icon name="pencil" size={14} /> Bearbeiten</DropdownMenu.Item>
        <DropdownMenu.Item class={menuItem} disabled={k.status !== 'active'} onSelect={() => toast('success', 'Key rotiert', 'Alter Key bleibt 24 Std. gültig.')}>
          <Icon name="refresh" size={14} /> Rotieren
        </DropdownMenu.Item>
        <DropdownMenu.Item class={menuItem} onSelect={() => navigator.clipboard?.writeText(k.id)}><Icon name="copy" size={14} /> ID kopieren</DropdownMenu.Item>
        <DropdownMenu.Separator class={menuSeparator} />
        <DropdownMenu.Item class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')} disabled={k.status !== 'active'} onSelect={() => askRevoke(k)}>
          <Icon name="ban" size={14} /> Sperren …
        </DropdownMenu.Item>
      </DropdownMenu.Content>
    </DropdownMenu.Portal>
  </DropdownMenu.Root>
{/snippet}

<PageHeader title="Keys" description="Virtuelle API-Keys für Anwendungen. Budgets und Ratenlimits greifen je Key zusätzlich zu Team- und Organisationslimits.">
  {#snippet actions()}
    <Button variant="secondary" icon="download">Export</Button>
    <Button variant="primary" icon="plus" onclick={() => (createOpen = true)} data-testid="create-key">Key erstellen</Button>
  {/snippet}
</PageHeader>

{#if res.state === 'unavailable'}
  <Card><EmptyState icon="key" title="Keys noch nicht verfügbar" text={res.reason} /></Card>
{:else}
  <div class="mb-4 grid grid-cols-2 gap-3 md:grid-cols-4">
    {#each [{ l: 'Aktive Keys', v: all.filter((k) => k.status === 'active').length }, { l: 'Laufen in 14 Tagen ab', v: all.filter(expiringSoon).length }, { l: 'Budget ≥ 80 %', v: all.filter((k) => k.budgetLimit && k.budgetUsed / k.budgetLimit >= 0.8).length }, { l: 'Gesperrt / abgelaufen', v: all.filter((k) => k.status !== 'active').length }] as s (s.l)}
      <div class="rounded-lg border border-line bg-surface px-4 py-3 shadow-card">
        <div class="text-sm text-fg-muted">{s.l}</div>
        <div class="num mt-1 text-xl font-semibold">{s.v}</div>
      </div>
    {/each}
  </div>
  <Card flush testid="keys-card">
    <DataTable
      {rows}
      {columns}
      bind:q
      key={(k) => k.id}
      caption="API-Keys"
      search={(k) => `${k.name} ${k.last4} ${teamName(k.teamId)}`}
      searchPlaceholder="Name, Team oder letzte 4 Zeichen …"
      sortId="used"
      asc={false}
      testid="keys-table"
      resetFilters={() => {
        status = 'all';
        ctx.teamId = '';
      }}
    >
      {#snippet toolbar()}
        <div class="w-40">
          <Select
            size="sm"
            label="Status"
            bind:value={status}
            items={[
              { value: 'all', label: 'Alle Status' },
              { value: 'active', label: 'Aktiv' },
              { value: 'revoked', label: 'Gesperrt' },
              { value: 'expired', label: 'Abgelaufen' },
            ]}
          />
        </div>
        <div class="w-44">
          <Select size="sm" label="Team" icon="users" bind:value={ctx.teamId} items={[{ value: '', label: 'Alle Teams' }, ...currentOrg().teams.map((t) => ({ value: t.id, label: t.name }))]} />
        </div>
      {/snippet}
      {#snippet empty()}
        <EmptyState icon="key" title="Noch keine Keys" text="Erstelle den ersten Key, um Anwendungen an das Gateway anzubinden.">
          <Button variant="primary" icon="plus" onclick={() => (createOpen = true)}>Key erstellen</Button>
        </EmptyState>
      {/snippet}
    </DataTable>
  </Card>
{/if}

<CreateKeyDialog bind:open={createOpen} />

<Dialog bind:open={revokeOpen} title="Key sperren?" size="sm" testid="revoke-dialog">
  <p class="text-base text-fg-muted">
    „<span class="font-medium text-fg">{revoking?.name}</span>“ wird sofort abgewiesen (HTTP 401). Laufende Streams werden beendet. Das lässt sich
    nicht rückgängig machen.
  </p>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (revokeOpen = false)}>Abbrechen</Button>
    <Button variant="danger" icon="ban" onclick={confirmRevoke}>Sperren</Button>
  {/snippet}
</Dialog>
