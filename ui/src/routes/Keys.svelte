<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Keys (AIGW-49, AIGW-77): Liste mit serverseitiger Pagination, Suche und Filtern (Status, Team, Organisation –
  // D11/F139), Erstellen mit einmaliger Klartext-Anzeige, Zeilenmenü (Bearbeiten, Rotieren, Sperren/Entsperren,
  // endgültig Widerrufen, Löschen – F37) und Detailseite je Key (/keys/{name}, D8).
  // Schreibaktionen nur, wenn die Rolle sie erlaubt (rbac.svelte.ts); der Server prüft immer selbst.
  import { DropdownMenu } from 'bits-ui';
  import { untrack } from 'svelte';
  import { listPage, type Query } from '../lib/api/client';
  import { apiKeys, type ApiKey, type S } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import { Loader } from '../lib/admin/errors.svelte';
  import { Paged } from '../lib/admin/paged.svelte';
  import SecretDialog from '../lib/admin/SecretDialog.svelte';
  import { ctx, currentOrg, orgOfTeam, teamLabel } from '../lib/context.svelte';
  import { ago, dateTime, day, short } from '../lib/format';
  import { detailPath } from '../lib/nav';
  import { canWriteKey, canWriteSomething } from '../lib/rbac.svelte';
  import { href, navigate, route } from '../lib/router.svelte';
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
  import KeyActionDialog from './keys/KeyActionDialog.svelte';
  import KeyFormDialog from './keys/KeyFormDialog.svelte';
  import { keyStatus, STATE_FILTER, type KeyAction } from './keys/keyState';
  import RotateKeyDialog from './keys/RotateKeyDialog.svelte';

  const params = new URLSearchParams(route.search);
  let formOpen = $state(params.get('new') === '1');
  let editName = $state<string | null>(null);
  if (params.get('team')) ctx.teamId = params.get('team') ?? '';
  let status = $state(params.get('state') ?? '');
  let secret = $state<string | null>(null);
  let secretTitle = $state('');
  let snippetModel = $state('gpt-4o');
  let rotateOpen = $state(false);
  let rotating = $state<string | null>(null);
  let act = $state<{ action: KeyAction; name: string } | null>(null);
  let actOpen = $state(false);

  // Serverseitige Filter (F42/F139): Status, Team, Organisation des Kontext-Umschalters.
  const filters = (): Query => ({ state: status || undefined, team: ctx.teamId || undefined, organization: ctx.orgId || undefined });
  const keys = new Paged<ApiKey>((q, limit, cursor) => listPage<ApiKey>(apiKeys.path, q, limit, cursor), filters, { q: params.get('q') ?? '' });
  // Kennzahlen je Zustand (höchstens 500 gezählt, darüber „500+“).
  const counts = new Loader(async () => {
    const count = async (state: S['ApiKeyState']) => {
      const p = await listPage<ApiKey>(apiKeys.path, { team: ctx.teamId || undefined, organization: ctx.orgId || undefined, state }, 500);
      return p.nextCursor ? '500+' : String(p.items.length);
    };
    const [active, suspended, revoked] = await Promise.all([count('active'), count('suspended'), count('revoked')]);
    return { active, suspended, revoked };
  });
  let first = true;
  $effect(() => {
    void ctx.teamId;
    void ctx.orgId;
    void status;
    if (first) {
      first = false;
      return;
    }
    untrack(() => {
      void keys.first();
      void counts.reload();
    });
  });
  function reload() {
    void keys.reload();
    void counts.reload();
  }

  const writable = (k: ApiKey) => canWriteKey(k.spec.team, k.spec.organization ?? orgOfTeam(k.spec.team));

  function created(r: S['CreatedApiKey']) {
    secretTitle = `Key „${r.key.metadata.name}“ erstellt`;
    snippetModel = r.key.spec.allowedModels?.[0] ?? 'gpt-4o';
    secret = r.secret;
    reload();
  }
  function rotated(r: S['RotatedApiKey']) {
    secretTitle = `Key „${r.newKey.key.metadata.name}“ erstellt (Rotation)`;
    snippetModel = r.newKey.key.spec.allowedModels?.[0] ?? 'gpt-4o';
    secret = r.newKey.secret;
    toast('info', 'Alter Key läuft ab', r.previousKey.spec.expiresAt ? `„${r.previousKey.metadata.name}“ gilt bis ${dateTime(Date.parse(r.previousKey.spec.expiresAt))}.` : undefined);
    reload();
  }
  function edit(k: ApiKey) {
    editName = k.metadata.name;
    formOpen = true;
  }
  function create() {
    editName = null;
    formOpen = true;
  }
  function ask(action: KeyAction, k: ApiKey) {
    act = { action, name: k.metadata.name };
    actOpen = true;
  }

  const limitText = (k: ApiKey) => {
    const l = (m: S['LimitMetric']) => k.spec.limits?.find((x) => x.metric === m && x.window === 'minute' && !x.modelGroup)?.limit;
    const rpm = l('requests');
    const tpm = l('tokens');
    return [rpm ? `${short(rpm)} RPM` : null, tpm ? `${short(tpm)} TPM` : null].filter(Boolean).join(' · ') || 'keine Ratenlimits';
  };
  const PERIOD: Record<S['BudgetPeriod'], string> = { day: 'Tag', week: 'Woche', month: 'Monat', total: 'gesamt' };

  const columns: Column<ApiKey>[] = [
    { id: 'name', label: 'Name', cell: nameCell },
    { id: 'team', label: 'Team', cell: teamCell, from: 'md' },
    { id: 'models', label: 'Modelle', cell: modelsCell, from: 'xl' },
    { id: 'limits', label: 'Budget & Limits', cell: limitsCell, from: 'lg' },
    { id: 'expires', label: 'Ablauf', cell: expiresCell, from: 'lg' },
    { id: 'used', label: 'Zuletzt genutzt', cell: usedCell, from: 'sm' },
    { id: 'status', label: 'Status', cell: statusCell },
    { id: 'actions', label: 'Aktionen', cell: actionsCell, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
</script>

{#snippet nameCell(k: ApiKey)}
  <div class="flex min-w-0 flex-col">
    <a class="truncate font-medium text-fg hover:text-accent-text" href={href(detailPath('key', k.metadata.name))}>{k.metadata.name}</a>
    <span class="font-mono text-xs text-fg-muted" data-testid="key-prefix">sk-aigw-{k.status.prefix}…</span>
  </div>
{/snippet}
{#snippet teamCell(k: ApiKey)}<span class="text-fg-muted">{k.spec.team ? teamLabel(k.spec.team) : `kein Team · ${k.spec.organization}`}</span>{/snippet}
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
  {@const st = keyStatus(k)}
  <Badge tone={st.tone} dot>{st.label}</Badge>
{/snippet}
{#snippet actionsCell(k: ApiKey)}
  <DropdownMenu.Root>
    <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {k.metadata.name}"><Icon name="more" /></DropdownMenu.Trigger>
    <DropdownMenu.Portal>
      <DropdownMenu.Content class={cx(popover, 'w-48')} align="end" sideOffset={4}>
        <DropdownMenu.Item class={menuItem} onSelect={() => navigate(detailPath('key', k.metadata.name))}><Icon name="eye" size={14} /> Details</DropdownMenu.Item>
        {#if writable(k) && k.status.state !== 'revoked'}
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
        {#if writable(k) && k.status.state !== 'revoked'}
          <DropdownMenu.Separator class={menuSeparator} />
          {#if k.status.state === 'active'}
            <DropdownMenu.Item class={menuItem} onSelect={() => ask('suspend', k)}><Icon name="ban" size={14} /> Sperren …</DropdownMenu.Item>
          {:else}
            <DropdownMenu.Item class={menuItem} onSelect={() => ask('resume', k)}><Icon name="check-circle" size={14} /> Entsperren …</DropdownMenu.Item>
          {/if}
          <DropdownMenu.Item class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')} onSelect={() => ask('revoke', k)}><Icon name="x-circle" size={14} /> Endgültig widerrufen …</DropdownMenu.Item>
        {/if}
        {#if writable(k)}
          <DropdownMenu.Item class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')} onSelect={() => ask('delete', k)}><Icon name="trash" size={14} /> Löschen …</DropdownMenu.Item>
        {/if}
      </DropdownMenu.Content>
    </DropdownMenu.Portal>
  </DropdownMenu.Root>
{/snippet}

<PageHeader title="Keys" description="Virtuelle API-Keys für Anwendungen. Budgets und Ratenlimits greifen je Key zusätzlich zu Team- und Organisationslimits.">
  {#snippet actions()}
    <Button variant="secondary" icon="refresh" onclick={reload} loading={keys.loading} aria-label="Neu laden">Neu laden</Button>
    {#if canWriteSomething()}
      <Button variant="primary" icon="plus" onclick={create} data-testid="create-key">Key erstellen</Button>
    {/if}
  {/snippet}
</PageHeader>


  <div class="mb-4 grid grid-cols-3 gap-3" data-testid="key-counts">
    {#each [{ l: 'Aktiv', k: 'active' }, { l: 'Gesperrt', k: 'suspended' }, { l: 'Widerrufen', k: 'revoked' }] as const as c (c.k)}
      <button type="button" class={['rounded-lg border bg-surface px-4 py-3 text-left shadow-card hover:bg-surface-hover', status === c.k ? 'border-accent' : 'border-line']} onclick={() => (status = status === c.k ? '' : c.k)}>
        <div class="text-sm text-fg-muted">{c.l}</div>
        <div class="num mt-1 text-xl font-semibold">{counts.data ? counts.data[c.k] : '–'}</div>
      </button>
    {/each}
  </div>
  <Card flush testid="keys-card">
    <AsyncState loader={keys}>
      {#snippet children(rows)}
        <DataTable
          {rows}
          {columns}
          server={keys}
          filtered={!!status || !!ctx.teamId}
          key={(k) => k.metadata.name}
          caption="API-Keys"
          search={() => ''}
          searchPlaceholder="Name oder Präfix …"
          testid="keys-table"
          onrowclick={(k) => navigate(detailPath('key', k.metadata.name))}
          resetFilters={() => {
            status = '';
            ctx.teamId = '';
          }}
        >
          {#snippet toolbar()}
            <div class="w-40">
              <Select size="sm" label="Status" bind:value={status} items={STATE_FILTER} />
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

<KeyFormDialog bind:open={formOpen} {editName} oncreated={created} onsaved={() => (toast('success', 'Key gespeichert'), reload())} />
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

<KeyActionDialog bind:open={actOpen} action={act?.action ?? null} name={act?.name ?? null} ondone={reload} />
