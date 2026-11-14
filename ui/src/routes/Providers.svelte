<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Anbieter (AIGW-49, AIGW-84):
  // - Übersicht je Anbieter + Endpunkt aus den Deployments (auch ohne Zugangsdaten, z. B. Entra ID per Managed Identity
  //   mit Inline-Einstellungen): Anmeldeart, Deployments, Modellgruppen, Zustand dieser Instanz (GET /providers).
  // - Anbieter-Zugangsdaten ohne Werte (nur Feldnamen, Entra-ID-Angaben, Verschlüsselungs-Key-ID), Verwendung durch
  //   Deployments; Anlegen, Werte ändern (PATCH), alle Werte ersetzen (PUT), Löschen. Schreiben: globaler Admin.
  import { DropdownMenu } from 'bits-ui';
  import { authLabel, credentials, deployments, entraLabel, HEALTH, PROVIDERS, providerLabel, providerOverview, type Credential, type ProviderEndpoint } from '../lib/api/resources';
  import { detailPath } from '../lib/nav';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf, Loader } from '../lib/admin/errors.svelte';
  import { ago } from '../lib/format';
  import { canWrite } from '../lib/rbac.svelte';
  import { href } from '../lib/router.svelte';
  import { toast } from '../lib/toast.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../lib/ui/DataTable.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Select from '../lib/ui/Select.svelte';
  import { btn, cx, menuItem, menuSeparator, popover } from '../lib/ui/styles';
  import CredentialFormDialog, { type CredentialMode } from './providers/CredentialFormDialog.svelte';

  const data = new Loader(async () => {
    const [c, d] = await Promise.all([credentials.list(), deployments.list()]);
    return { creds: c, deps: d };
  });
  const overview = new Loader(providerOverview);
  let provider = $state('');
  let epQ = $state('');
  let q = $state('');
  let formOpen = $state(false);
  let mode = $state<CredentialMode>('create');
  let target = $state<string | null>(null);
  let del = $state<string | null>(null);
  let delOpen = $state(false);

  const writable = $derived(canWrite({ kind: 'catalog' }));
  const usedBy = (c: Credential) => (data.data?.deps ?? []).filter((d) => d.spec.credential === c.metadata.name);
  const rows = $derived((data.data?.creds ?? []).filter((c) => !provider || c.spec.provider === provider));

  function open(m: CredentialMode, name: string | null) {
    mode = m;
    target = name;
    formOpen = true;
  }
  async function runDelete(): Promise<string | null> {
    if (!del) return null;
    try {
      const { etag } = await credentials.get(del);
      await credentials.remove(del, etag);
      toast('success', 'Zugangsdaten gelöscht', del);
      void data.reload();
      void overview.reload();
      return null;
    } catch (e) {
      return formErrorOf(e, 'Zugangsdaten löschen', () => void data.reload());
    }
  }

  const epRows = $derived((overview.data?.items ?? []).filter((e) => !provider || e.provider === provider));
  const epKey = (e: ProviderEndpoint) => `${e.provider} ${e.endpoint ?? ''}`;
  const STATE_ORDER = { down: 0, unusable: 1, degraded: 2, unknown: 3, ok: 4, disabled: 5 } as const;
  const epColumns: Column<ProviderEndpoint>[] = [
    { id: 'endpoint', label: 'Anbieter / Endpunkt', cell: epCell, sort: (e) => `${providerLabel(e.provider)} ${e.endpoint ?? ''}` },
    { id: 'auth', label: 'Anmeldung', cell: epAuthCell, from: 'sm' },
    { id: 'deployments', label: 'Deployments', cell: epDepsCell, sort: (e) => e.deployments.length, from: 'md' },
    { id: 'groups', label: 'Modellgruppen', cell: epGroupsCell, sort: (e) => e.modelGroups.length, from: 'lg' },
    { id: 'state', label: 'Zustand', cell: epStateCell, sort: (e) => STATE_ORDER[e.state] },
  ];

  const columns: Column<Credential>[] = [
    { id: 'name', label: 'Name', cell: nameCell, sort: (c) => c.metadata.name },
    { id: 'provider', label: 'Anbieter', cell: providerCell, sort: (c) => providerLabel(c.spec.provider) },
    { id: 'fields', label: 'Anmeldung / Felder', cell: fieldsCell, from: 'md' },
    { id: 'used', label: 'Verwendet von', cell: usedCell, sort: (c) => usedBy(c).length, from: 'lg' },
    { id: 'updated', label: 'Geändert', cell: updatedCell, sort: (c) => c.metadata.updatedAt ?? '', from: 'sm' },
    { id: 'actions', label: 'Aktionen', cell: actionsCell, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
</script>

{#snippet nameCell(c: Credential)}
  <span class="font-mono text-sm font-medium text-fg">{c.metadata.name}</span>
  {#if Object.keys(c.metadata.labels ?? {}).length}
    <span class="mt-0.5 flex flex-wrap gap-1">{#each Object.entries(c.metadata.labels ?? {}) as [k, v] (k)}<Badge tone="outline" mono>{k}={v}</Badge>{/each}</span>
  {/if}
{/snippet}
{#snippet providerCell(c: Credential)}<span class="text-sm">{providerLabel(c.spec.provider)}</span>{/snippet}
{#snippet fieldsCell(c: Credential)}
  <span class="flex flex-wrap items-center gap-1">
    {#if c.spec.entra}
      <Badge tone="info" icon="shield">Entra ID · {entraLabel(c.spec.entra.credential)}</Badge>
      {#if c.spec.entra.clientId}<Badge mono tone="outline" class="max-w-48 truncate">client {c.spec.entra.clientId}</Badge>{/if}
    {/if}
    {#each c.spec.fields as f (f)}<Badge mono icon="lock">{f}</Badge>{:else}{#if !c.spec.entra}<span class="text-sm text-fg-muted">keine Werte</span>{/if}{/each}
  </span>
{/snippet}
{#snippet epCell(e: ProviderEndpoint)}
  <span class="block text-sm font-medium text-fg">{providerLabel(e.provider)}</span>
  {#if e.endpoint}<span class="block max-w-xs truncate font-mono text-xs text-fg-muted" title={e.endpoint}>{e.endpoint}</span>
  {:else}<span class="block text-xs text-fg-subtle">Standard-Endpunkt</span>{/if}
{/snippet}
{#snippet epAuthCell(e: ProviderEndpoint)}
  <span class="flex flex-col gap-1">
    {#each e.auth as a, i (i)}
      <span class="flex flex-wrap items-center gap-1">
        <Badge tone={a.kind === 'entraId' ? 'info' : a.kind === 'none' ? 'outline' : 'neutral'} icon={a.kind === 'entraId' ? 'shield' : a.kind === 'none' ? undefined : 'key'}>{authLabel(a)}</Badge>
        {#if a.credential}<Badge mono tone="outline">{a.credential}</Badge>{:else if a.kind !== 'none'}<span class="text-xs text-fg-subtle" title="Anmeldung in den Deployment-Einstellungen">inline</span>{/if}
        {#if a.clientId}<span class="max-w-40 truncate font-mono text-xs text-fg-muted" title="Client-ID {a.clientId}">{a.clientId}</span>{/if}
      </span>
    {/each}
  </span>
{/snippet}
{#snippet epDepsCell(e: ProviderEndpoint)}
  <span class="flex flex-wrap gap-1">
    {#each e.deployments.slice(0, 4) as d (d.name)}
      <a href={href(detailPath('deployment', d.name))} class="rounded-sm" title="{d.modelGroup} · {d.providerModel} · {HEALTH[d.state].label}{d.error ? ` – ${d.error}` : ''}">
        <Badge mono dot tone={HEALTH[d.state].tone}>{d.name}</Badge>
      </a>
    {/each}
    {#if e.deployments.length > 4}<Badge tone="outline">+{e.deployments.length - 4}</Badge>{/if}
  </span>
{/snippet}
{#snippet epGroupsCell(e: ProviderEndpoint)}
  <span class="flex flex-wrap gap-1">
    {#each e.modelGroups.slice(0, 3) as g (g)}<a href={href(detailPath('model-group', g))} class="rounded-sm"><Badge mono tone="outline">{g}</Badge></a>{/each}
    {#if e.modelGroups.length > 3}<Badge tone="outline">+{e.modelGroups.length - 3}</Badge>{/if}
  </span>
{/snippet}
{#snippet epStateCell(e: ProviderEndpoint)}
  {@const problems = e.deployments.filter((d) => d.state !== 'ok' && d.state !== 'disabled').length}
  <span class="flex flex-col items-start gap-0.5">
    <Badge dot tone={HEALTH[e.state].tone}>{HEALTH[e.state].label}</Badge>
    {#if problems && e.state !== 'unknown'}<span class="text-xs text-fg-muted">{problems} von {e.deployments.length} betroffen</span>{/if}
  </span>
{/snippet}
{#snippet usedCell(c: Credential)}
  {@const u = usedBy(c)}
  {#if u.length === 0}<span class="text-sm text-fg-muted">nicht verwendet</span>
  {:else}
    <span class="flex flex-wrap gap-1">
      {#each u.slice(0, 3) as d (d.metadata.name)}<a href={href(`/models?group=${encodeURIComponent(d.spec.modelGroup)}`)} class="rounded-sm"><Badge mono>{d.metadata.name}</Badge></a>{/each}
      {#if u.length > 3}<Badge tone="outline">+{u.length - 3}</Badge>{/if}
    </span>
  {/if}
{/snippet}
{#snippet updatedCell(c: Credential)}<span class="text-sm text-fg-muted" title="Master-Key {c.status.keyId}">{c.metadata.updatedAt ? ago(Date.parse(c.metadata.updatedAt)) : '–'}</span>{/snippet}
{#snippet actionsCell(c: Credential)}
  {#if writable}
    <DropdownMenu.Root>
      <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {c.metadata.name}"><Icon name="more" /></DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content class={cx(popover, 'w-52')} align="end" sideOffset={4}>
          <DropdownMenu.Item class={menuItem} onSelect={() => open('patch', c.metadata.name)}><Icon name="pencil" size={14} /> Werte ändern …</DropdownMenu.Item>
          <DropdownMenu.Item class={menuItem} onSelect={() => open('replace', c.metadata.name)}><Icon name="refresh" size={14} /> Alle Werte ersetzen …</DropdownMenu.Item>
          <DropdownMenu.Separator class={menuSeparator} />
          <DropdownMenu.Item
            class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')}
            onSelect={() => {
              del = c.metadata.name;
              delOpen = true;
            }}><Icon name="trash" size={14} /> Löschen …</DropdownMenu.Item
          >
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  {/if}
{/snippet}

<PageHeader title="Anbieter" description="Upstream-Anbieter je Endpunkt mit Anmeldeart und Zustand, darunter die Zugangsdaten. Geheimwerte sind nur schreibbar: Sie werden beim Speichern mit dem Master-Key verschlüsselt und nie wieder ausgegeben.">
  {#snippet actions()}
    {#if writable}<Button variant="primary" icon="plus" onclick={() => open('create', null)} data-testid="create-credential">Zugangsdaten anlegen</Button>{/if}
  {/snippet}
</PageHeader>

<div class="flex flex-col gap-6">
  <Card flush testid="provider-overview" title="Endpunkte" description="Aus den Deployments abgeleitet – auch Anbieter ohne Zugangsdaten (z. B. Entra ID per Managed Identity). Zustand dieser Gateway-Instanz.">
    <AsyncState loader={overview}>
      {#snippet children()}
        {#if overview.data && !overview.data.runtime}<p class="border-b border-line px-4 py-2 text-sm text-fg-muted">Kein Laufzeitzustand verfügbar – nur Konfiguration.</p>{/if}
        <DataTable
          rows={epRows}
          columns={epColumns}
          bind:q={epQ}
          key={epKey}
          caption="Anbieter-Endpunkte"
          search={(e) => `${e.provider} ${providerLabel(e.provider)} ${e.endpoint ?? ''} ${e.auth.map((a) => `${authLabel(a)} ${a.credential ?? ''}`).join(' ')} ${e.deployments.map((d) => d.name).join(' ')} ${e.modelGroups.join(' ')}`}
          searchPlaceholder="Anbieter, Endpunkt, Deployment …"
          sortId="state"
          testid="provider-endpoints"
          resetFilters={() => (provider = '')}
        >
          {#snippet empty()}
            <EmptyState icon="server" title="Noch keine Deployments" text="Die Übersicht entsteht aus den Deployments unter „Modelle & Routen“ (oder per GitOps).">
              <a class={btn('secondary')} href={href('/models')}>Zu Modelle & Routen</a>
            </EmptyState>
          {/snippet}
        </DataTable>
      {/snippet}
    </AsyncState>
  </Card>

  <Card flush testid="credentials-card" title="Zugangsdaten">
    <AsyncState loader={data}>
      {#snippet children()}
        <DataTable
          {rows}
          {columns}
          bind:q
          key={(c) => c.metadata.name}
          caption="Anbieter-Zugangsdaten"
          search={(c) => `${c.metadata.name} ${c.spec.provider} ${c.spec.fields.join(' ')} ${c.spec.entra ? `entra ${c.spec.entra.credential} ${c.spec.entra.clientId ?? ''}` : ''}`}
          searchPlaceholder="Name, Anbieter oder Feld …"
          sortId="name"
          testid="credentials-table"
          resetFilters={() => (provider = '')}
        >
          {#snippet toolbar()}
            <div class="w-48">
              <Select size="sm" label="Anbieter" bind:value={provider} items={[{ value: '', label: 'Alle Anbieter' }, ...PROVIDERS.map((p) => ({ value: p.value, label: p.label }))]} />
            </div>
          {/snippet}
          {#snippet empty()}
            <EmptyState icon="plug" title="Noch keine Zugangsdaten" text="Deployments brauchen Zugangsdaten des jeweiligen Anbieters (außer Mock/Ollama ohne Authentifizierung).">
              {#if writable}<Button variant="primary" icon="plus" onclick={() => open('create', null)}>Zugangsdaten anlegen</Button>{/if}
            </EmptyState>
          {/snippet}
        </DataTable>
      {/snippet}
    </AsyncState>
  </Card>
</div>

<CredentialFormDialog
  bind:open={formOpen}
  {mode}
  name={target}
  onsaved={(n) => {
    toast('success', 'Zugangsdaten gespeichert', n);
    void data.reload();
    void overview.reload();
  }}
/>
<ConfirmDialog bind:open={delOpen} title="Zugangsdaten löschen?" confirmLabel="Löschen" onconfirm={runDelete}>
  <p>„<span class="font-mono text-fg">{del}</span>“ wird mit allen verschlüsselten Werten gelöscht. Nur möglich, wenn kein Deployment sie verwendet.</p>
</ConfirmDialog>
