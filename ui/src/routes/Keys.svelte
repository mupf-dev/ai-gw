<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Keys (AIGW-49): Liste aus der Admin-API (Filter Status/Team serverseitig, Suche/Sortierung/Pagination in der
  // Tabelle), Erstellen mit einmaliger Klartext-Anzeige, Bearbeiten, Rotieren, Sperren/Entsperren, Löschen.
  // Schreibaktionen nur, wenn die Rolle sie erlaubt (rbac.svelte.ts); der Server prüft immer selbst.
  import { DropdownMenu } from 'bits-ui';
  import { untrack } from 'svelte';
  import { apiKeys, type ApiKey, type S } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf, Loader } from '../lib/admin/errors.svelte';
  import SecretDialog from '../lib/admin/SecretDialog.svelte';
  import { catalog, ctx, currentOrg, orgOfTeam, teamLabel } from '../lib/context.svelte';
  import { ago, dateTime, day, short } from '../lib/format';
  import { canWriteKey, canWriteSomething } from '../lib/rbac.svelte';
  import { route } from '../lib/router.svelte';
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
  import KeyFormDialog from './keys/KeyFormDialog.svelte';
  import RotateKeyDialog from './keys/RotateKeyDialog.svelte';

  const params = new URLSearchParams(route.search);
  let formOpen = $state(params.get('new') === '1');
  let editName = $state<string | null>(null);
  let q = $state(params.get('q') ?? '');
  if (params.get('team')) ctx.teamId = params.get('team') ?? '';
  let status = $state('all');
  let secret = $state<string | null>(null);
  let secretTitle = $state('');
  let snippetModel = $state('gpt-4o');
  let rotateOpen = $state(false);
  let rotating = $state<string | null>(null);
  let confirm = $state<{ kind: 'revoke' | 'reactivate' | 'delete'; key: ApiKey } | null>(null);
  let confirmOpen = $state(false);

  const DAY = 86_400_000;
  const expired = (k: ApiKey) => !!k.spec.expiresAt && Date.parse(k.spec.expiresAt) <= Date.now();
  const expiringSoon = (k: ApiKey) => k.status.state === 'active' && !!k.spec.expiresAt && !expired(k) && Date.parse(k.spec.expiresAt) - Date.now() < 14 * DAY;

  const keys = new Loader<ApiKey[]>(() =>
    apiKeys.list({ team: ctx.teamId || undefined, state: status === 'revoked' ? 'revoked' : status === 'all' ? undefined : 'active' }),
  );
  // Serverseitige Filter: bei Änderung neu laden.
  let first = true;
  $effect(() => {
    void ctx.teamId;
    void status;
    if (first) {
      first = false;
      return;
    }
    untrack(() => void keys.reload());
  });

  const inOrg = (k: ApiKey) => !ctx.orgId || !k.spec.team || orgOfTeam(k.spec.team) === ctx.orgId || !catalog.loaded;
  const rows = $derived(
    (keys.data ?? []).filter((k) => inOrg(k) && (status !== 'expired' || expired(k)) && (status !== 'active' || !expired(k))),
  );
  // Kennzahlen im aktuellen Organisationskontext (wie die Tabelle).
  const all = $derived((keys.data ?? []).filter(inOrg));
  const writable = (k: ApiKey) => canWriteKey(k.spec.team, orgOfTeam(k.spec.team));

  function created(r: S['CreatedApiKey']) {
    secretTitle = `Key „${r.key.metadata.name}“ erstellt`;
    snippetModel = r.key.spec.allowedModels?.[0] ?? 'gpt-4o';
    secret = r.secret;
    void keys.reload();
  }
  function rotated(r: S['RotatedApiKey']) {
    secretTitle = `Key „${r.newKey.key.metadata.name}“ erstellt (Rotation)`;
    snippetModel = r.newKey.key.spec.allowedModels?.[0] ?? 'gpt-4o';
    secret = r.newKey.secret;
    toast('info', 'Alter Key läuft ab', r.previousKey.spec.expiresAt ? `„${r.previousKey.metadata.name}“ gilt bis ${dateTime(Date.parse(r.previousKey.spec.expiresAt))}.` : undefined);
    void keys.reload();
  }
  function edit(k: ApiKey) {
    editName = k.metadata.name;
    formOpen = true;
  }
  function create() {
    editName = null;
    formOpen = true;
  }
  function ask(kind: 'revoke' | 'reactivate' | 'delete', key: ApiKey) {
    confirm = { kind, key };
    confirmOpen = true;
  }
  async function runConfirm(): Promise<string | null> {
    if (!confirm) return null;
    const { kind, key } = confirm;
    const name = key.metadata.name;
    const action = { revoke: 'Key sperren', reactivate: 'Key entsperren', delete: 'Key löschen' }[kind];
    try {
      const { etag } = await apiKeys.get(name);
      if (kind === 'revoke') await apiKeys.revoke(name, etag);
      else if (kind === 'reactivate') await apiKeys.reactivate(name, etag);
      else await apiKeys.remove(name, etag);
      toast('success', { revoke: 'Key gesperrt', reactivate: 'Key entsperrt', delete: 'Key gelöscht' }[kind], `„${name}“`);
      void keys.reload();
      return null;
    } catch (e) {
      return formErrorOf(e, action, () => void keys.reload());
    }
  }

  const limitText = (k: ApiKey) => {
    const l = (m: S['LimitMetric']) => k.spec.limits?.find((x) => x.metric === m && x.window === 'minute' && !x.modelGroup)?.limit;
    const rpm = l('requests');
    const tpm = l('tokens');
    return [rpm ? `${short(rpm)} RPM` : null, tpm ? `${short(tpm)} TPM` : null].filter(Boolean).join(' · ') || 'keine Ratenlimits';
  };
  const PERIOD: Record<S['BudgetPeriod'], string> = { day: 'Tag', week: 'Woche', month: 'Monat', total: 'gesamt' };

  const columns: Column<ApiKey>[] = [
    { id: 'name', label: 'Name', cell: nameCell, sort: (k) => k.metadata.name },
    { id: 'team', label: 'Team', cell: teamCell, sort: (k) => teamLabel(k.spec.team), from: 'md' },
    { id: 'models', label: 'Modelle', cell: modelsCell, from: 'xl' },
    { id: 'limits', label: 'Budget & Limits', cell: limitsCell, from: 'lg' },
    { id: 'expires', label: 'Ablauf', cell: expiresCell, sort: (k) => (k.spec.expiresAt ? Date.parse(k.spec.expiresAt) : Infinity), from: 'lg' },
    { id: 'used', label: 'Zuletzt genutzt', cell: usedCell, sort: (k) => (k.status.lastUsedAt ? Date.parse(k.status.lastUsedAt) : 0), from: 'sm' },
    { id: 'status', label: 'Status', cell: statusCell, sort: (k) => (k.status.state === 'revoked' ? 2 : expired(k) ? 1 : 0) },
    { id: 'actions', label: 'Aktionen', cell: actionsCell, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
</script>

{#snippet nameCell(k: ApiKey)}
  <div class="flex min-w-0 flex-col">
    <span class="truncate font-medium text-fg">{k.metadata.name}</span>
    <span class="font-mono text-xs text-fg-muted" data-testid="key-prefix">sk-aigw-{k.status.prefix}…</span>
  </div>
{/snippet}
{#snippet teamCell(k: ApiKey)}<span class="text-fg-muted">{k.spec.team ? teamLabel(k.spec.team) : 'kein Team'}</span>{/snippet}
{#snippet modelsCell(k: ApiKey)}
  {#if !k.spec.allowedModels?.length}
    <Badge tone="outline">Alle (Team)</Badge>
  {:else}
    <span class="flex flex-wrap gap-1">
      {#each k.spec.allowedModels.slice(0, 2) as m (m)}<Badge mono>{m}</Badge>{/each}
      {#if k.spec.allowedModels.length > 2}<Badge tone="outline">+{k.spec.allowedModels.length - 2}</Badge>{/if}
    </span>
  {/if}
{/snippet}
{#snippet limitsCell(k: ApiKey)}
  <span class="num flex flex-col text-xs whitespace-nowrap text-fg-muted">
    <span>{k.spec.budget ? `${k.spec.budget.amount} / ${PERIOD[k.spec.budget.period]}` : 'Budget unbegrenzt'}</span>
    <span>{limitText(k)}</span>
  </span>
{/snippet}
{#snippet expiresCell(k: ApiKey)}<span class="text-sm text-fg-muted">{k.spec.expiresAt ? day(Date.parse(k.spec.expiresAt)) : 'nie'}</span>{/snippet}
{#snippet usedCell(k: ApiKey)}<span class="text-sm text-fg-muted">{k.status.lastUsedAt ? ago(Date.parse(k.status.lastUsedAt)) : 'nie'}</span>{/snippet}
{#snippet statusCell(k: ApiKey)}
  {#if k.status.state === 'revoked'}<Badge tone="danger" dot>Gesperrt</Badge>
  {:else if expired(k)}<Badge tone="neutral" dot>Abgelaufen</Badge>
  {:else if expiringSoon(k)}<Badge tone="warning" dot>Läuft ab {day(Date.parse(k.spec.expiresAt ?? ''))}</Badge>
  {:else}<Badge tone="success" dot>Aktiv</Badge>{/if}
{/snippet}
{#snippet actionsCell(k: ApiKey)}
  <DropdownMenu.Root>
    <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {k.metadata.name}"><Icon name="more" /></DropdownMenu.Trigger>
    <DropdownMenu.Portal>
      <DropdownMenu.Content class={cx(popover, 'w-48')} align="end" sideOffset={4}>
        {#if writable(k)}
          <DropdownMenu.Item class={menuItem} onSelect={() => edit(k)}><Icon name="pencil" size={14} /> Bearbeiten</DropdownMenu.Item>
          <DropdownMenu.Item
            class={menuItem}
            disabled={k.status.state !== 'active'}
            onSelect={() => {
              rotating = k.metadata.name;
              rotateOpen = true;
            }}
          >
            <Icon name="refresh" size={14} /> Rotieren …
          </DropdownMenu.Item>
        {/if}
        <DropdownMenu.Item class={menuItem} onSelect={() => navigator.clipboard?.writeText(k.metadata.name)}><Icon name="copy" size={14} /> Name kopieren</DropdownMenu.Item>
        {#if writable(k)}
          <DropdownMenu.Separator class={menuSeparator} />
          {#if k.status.state === 'active'}
            <DropdownMenu.Item class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')} onSelect={() => ask('revoke', k)}><Icon name="ban" size={14} /> Sperren …</DropdownMenu.Item>
          {:else}
            <DropdownMenu.Item class={menuItem} onSelect={() => ask('reactivate', k)}><Icon name="check-circle" size={14} /> Entsperren …</DropdownMenu.Item>
          {/if}
          <DropdownMenu.Item class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')} onSelect={() => ask('delete', k)}><Icon name="trash" size={14} /> Löschen …</DropdownMenu.Item>
        {/if}
      </DropdownMenu.Content>
    </DropdownMenu.Portal>
  </DropdownMenu.Root>
{/snippet}

<PageHeader title="Keys" description="Virtuelle API-Keys für Anwendungen. Budgets und Ratenlimits greifen je Key zusätzlich zu Team- und Organisationslimits.">
  {#snippet actions()}
    <Button variant="secondary" icon="refresh" onclick={() => keys.reload()} loading={keys.loading} aria-label="Neu laden">Neu laden</Button>
    {#if canWriteSomething()}
      <Button variant="primary" icon="plus" onclick={create} data-testid="create-key">Key erstellen</Button>
    {/if}
  {/snippet}
</PageHeader>


  <div class="mb-4 grid grid-cols-2 gap-3 md:grid-cols-4">
    {#each [{ l: 'Aktive Keys', v: all.filter((k) => k.status.state === 'active' && !expired(k)).length }, { l: 'Laufen in 14 Tagen ab', v: all.filter(expiringSoon).length }, { l: 'Gesperrt', v: all.filter((k) => k.status.state === 'revoked').length }, { l: 'Abgelaufen', v: all.filter((k) => k.status.state === 'active' && expired(k)).length }] as s (s.l)}
      <div class="rounded-lg border border-line bg-surface px-4 py-3 shadow-card">
        <div class="text-sm text-fg-muted">{s.l}</div>
        <div class="num mt-1 text-xl font-semibold">{keys.data ? s.v : '–'}</div>
      </div>
    {/each}
  </div>
  <Card flush testid="keys-card">
    <AsyncState loader={keys}>
      {#snippet children()}
        <DataTable
          {rows}
          {columns}
          bind:q
          key={(k) => k.metadata.name}
          caption="API-Keys"
          search={(k) => `${k.metadata.name} ${k.status.prefix} ${teamLabel(k.spec.team)} ${k.spec.user ?? ''}`}
          searchPlaceholder="Name, Präfix, Team oder Nutzer …"
          sortId="name"
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
            <EmptyState icon="key" title="Noch keine Keys" text="Erstellen Sie den ersten Key, um Anwendungen an das Gateway anzubinden.">
              {#if canWriteSomething()}<Button variant="primary" icon="plus" onclick={create}>Key erstellen</Button>{/if}
            </EmptyState>
          {/snippet}
        </DataTable>
      {/snippet}
    </AsyncState>
  </Card>

<KeyFormDialog bind:open={formOpen} {editName} oncreated={created} onsaved={() => (toast('success', 'Key gespeichert'), keys.reload())} />
<RotateKeyDialog bind:open={rotateOpen} name={rotating} onrotated={rotated} />
<SecretDialog
  bind:secret
  ondone={() => toast('success', 'Key erstellt', 'Der Klartext ist verworfen; in der Liste steht nur das Präfix.')}
  title={secretTitle}
  description="Sofort einsatzbereit (nach dem Hot Reload, ca. 100 ms)."
  snippet={{
    title: 'Schnellstart (OpenAI-kompatibel)',
    code: `export AIGW_API_KEY=…   # Key von oben
curl "$AIGW_URL/v1/chat/completions" \\
  -H "Authorization: Bearer $AIGW_API_KEY" \\
  -H "Content-Type: application/json" \\
  -d '{"model": "${snippetModel}", "messages": [{"role": "user", "content": "Hallo!"}]}'`,
  }}
/>

<ConfirmDialog
  bind:open={confirmOpen}
  title={confirm?.kind === 'revoke' ? 'Key sperren?' : confirm?.kind === 'reactivate' ? 'Key entsperren?' : 'Key löschen?'}
  confirmLabel={confirm?.kind === 'revoke' ? 'Sperren' : confirm?.kind === 'reactivate' ? 'Entsperren' : 'Löschen'}
  icon={confirm?.kind === 'reactivate' ? 'check-circle' : confirm?.kind === 'revoke' ? 'ban' : 'trash'}
  danger={confirm?.kind !== 'reactivate'}
  onconfirm={runConfirm}
  testid="revoke-dialog"
>
  {#if confirm?.kind === 'revoke'}
    <p>„<span class="font-medium text-fg">{confirm.key.metadata.name}</span>“ wird nach dem Hot Reload sofort abgewiesen (HTTP 401). Entsperren ist möglich.</p>
  {:else if confirm?.kind === 'reactivate'}
    <p>„<span class="font-medium text-fg">{confirm.key.metadata.name}</span>“ wird wieder angenommen (sofern nicht abgelaufen).</p>
  {:else if confirm}
    <p>„<span class="font-medium text-fg">{confirm.key.metadata.name}</span>“ wird endgültig gelöscht und sofort abgewiesen. Das lässt sich nicht rückgängig machen.</p>
  {/if}
</ConfirmDialog>
