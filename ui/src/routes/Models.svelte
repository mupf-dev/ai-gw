<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Modelle & Routen (AIGW-49, AIGW-77): Modellgruppen mit serverseitiger Pagination und Suche (Name, Alias – D11);
  // Zeile → Detailseite /models/{name} (D8) mit Deployments, Einstellungen, Nutzung und Audit.
  // Schreiben nur für globale Admins (Katalog). Alte Links `?group=…` führen auf die Detailseite.
  import { listPage } from '../lib/api/client';
  import { modelGroups, ROUTING, type ModelGroup } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import { Paged } from '../lib/admin/paged.svelte';
  import { detailPath } from '../lib/nav';
  import { canWrite } from '../lib/rbac.svelte';
  import { href, navigate, route } from '../lib/router.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../lib/ui/DataTable.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import GroupFormDialog from './models/GroupFormDialog.svelte';

  const legacy = new URLSearchParams(route.search).get('group');
  if (legacy) navigate(detailPath('model-group', legacy), true);

  const groups = new Paged<ModelGroup>((q, l, c) => listPage(modelGroups.path, q, l, c));
  let groupOpen = $state(false);
  let allNames = $state<string[]>([]);
  const writable = $derived(canWrite({ kind: 'catalog' }));
  async function create() {
    allNames = (await modelGroups.list()).map((g) => g.metadata.name);
    groupOpen = true;
  }
  const strategy = (g: ModelGroup) => ROUTING.find((r) => r.value === (g.spec.routing ?? 'weighted'))?.label;

  const columns: Column<ModelGroup>[] = [
    { id: 'name', label: 'Modellgruppe', cell: nameCell },
    { id: 'routing', label: 'Strategie', cell: routingCell, from: 'sm' },
    { id: 'fallbacks', label: 'Fallback-Kette', cell: fallbackCell, from: 'md' },
    { id: 'limits', label: 'Limits', cell: limitsCell, from: 'lg' },
  ];
</script>

{#snippet nameCell(g: ModelGroup)}
  <a class="block font-mono text-sm font-medium text-fg hover:text-accent-text" href={href(detailPath('model-group', g.metadata.name))}>{g.metadata.name}</a>
  {#if g.spec.aliases?.length}<span class="mt-0.5 flex flex-wrap gap-1">{#each g.spec.aliases.slice(0, 3) as a (a)}<Badge mono tone="outline">{a}</Badge>{/each}{#if g.spec.aliases.length > 3}<Badge tone="outline">+{g.spec.aliases.length - 3}</Badge>{/if}</span>{/if}
{/snippet}
{#snippet routingCell(g: ModelGroup)}<span class="block text-sm">{strategy(g)}</span><span class="block text-xs text-fg-muted">{g.spec.capabilities?.length ? g.spec.capabilities.join(', ') : 'chat'}</span>{/snippet}
{#snippet fallbackCell(g: ModelGroup)}
  {#if g.spec.fallbacks?.length}<span class="font-mono text-sm text-fg-muted">→ {g.spec.fallbacks.map((f) => f.modelGroup).join(' → ')}</span>{:else}<span class="text-sm text-fg-subtle">–</span>{/if}
{/snippet}
{#snippet limitsCell(g: ModelGroup)}<span class="num text-sm text-fg-muted">{g.spec.limits?.length ? `${g.spec.limits.length} Limit(s)` : 'keine'}</span>{/snippet}

<PageHeader title="Modelle & Routen" description="Modellgruppen sind die öffentlichen Modellnamen der Clients. Dahinter wählt das Gateway ein Deployment nach Priorität und Strategie; bei Fehlern greift die Fallback-Kette.">
  {#snippet actions()}
    {#if writable}
      <Button variant="primary" icon="plus" onclick={create} data-testid="create-group">Modellgruppe anlegen</Button>
    {/if}
  {/snippet}
</PageHeader>

<Card flush>
  <AsyncState loader={groups}>
    {#snippet children(rows)}
      <DataTable
        {rows}
        {columns}
        server={groups}
        key={(g) => g.metadata.name}
        caption="Modellgruppen"
        search={() => ''}
        searchPlaceholder="Name oder Alias …"
        testid="group-list"
        onrowclick={(g) => navigate(detailPath('model-group', g.metadata.name))}
      >
        {#snippet empty()}
          <EmptyState icon="route" title="Noch keine Modellgruppen" text="Legen Sie eine Modellgruppe an und hängen Sie Deployments (Anbieter-Modelle) dahinter. Alternativ per GitOps (Einstellungen → GitOps).">
            {#if writable}<Button variant="primary" icon="plus" onclick={create}>Modellgruppe anlegen</Button>{/if}
          </EmptyState>
        {/snippet}
      </DataTable>
    {/snippet}
  </AsyncState>
</Card>

<GroupFormDialog bind:open={groupOpen} groups={allNames} onsaved={(n) => navigate(detailPath('model-group', n))} />
