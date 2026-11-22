<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Tools (AIGW-87): Inventar der Tools, die Anwendungen in Anfragen mitschicken (Name, Art, Herkunft/MCP-Server,
  // Client, Team/Key, Nutzung, Hash von Beschreibung und Schema) mit wirksamem Status (neu, freigegeben, gesperrt,
  // wartet auf Freigabe) und Markierungen „geändert“/„verdächtig“. Freigeben/Sperren je Tool und Ebene schreibt die
  // Entscheidungs-Richtlinien (tools-allow-…/tools-block-…); Detail mit Beschreibung, Schema, Hash-Verlauf und
  // passenden Richtlinien. Zweiter Reiter: Tool-Richtlinien (CRUD, Muster mit `!`-Ausschluss).
  import { DropdownMenu, Tabs } from 'bits-ui';
  import { untrack } from 'svelte';
  import { listPage } from '../lib/api/client';
  import { NAME_HINT, toolPolicies, tools, type S, type ToolItem, type ToolPolicy } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf, handleError, splitList } from '../lib/admin/errors.svelte';
  import FormDialog from '../lib/admin/FormDialog.svelte';
  import { Paged } from '../lib/admin/paged.svelte';
  import { catalog, orgOfTeam } from '../lib/context.svelte';
  import { dateTime, num } from '../lib/format';
  import { canWrite, canWriteSomething } from '../lib/rbac.svelte';
  import { toast } from '../lib/toast.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import CodeBlock from '../lib/ui/CodeBlock.svelte';
  import DataTable, { type Column } from '../lib/ui/DataTable.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import Field from '../lib/ui/Field.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import Input from '../lib/ui/Input.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Select from '../lib/ui/Select.svelte';
  import Sheet from '../lib/ui/Sheet.svelte';
  import { btn, cx, menuItem, menuSeparator, popover, tabsList, tabsTrigger, textarea } from '../lib/ui/styles';

  let tab = $state('inventory');

  // ------------------------------------------------------------ Inventar
  let status = $state('');
  let origin = $state('');
  let client = $state('');
  let team = $state('');
  const list = new Paged<ToolItem>(
    (q, l, c) => listPage(tools.path, q, l, c),
    () => ({ status: status || undefined, origin: origin.trim() || undefined, client: client.trim() || undefined, team: team.trim() || undefined }),
  );
  let first = true;
  $effect(() => {
    void status;
    void origin;
    void client;
    void team;
    if (first) {
      first = false;
      return;
    }
    untrack(() => void list.first());
  });

  const STATUS: Record<S['ToolStatus'], { label: string; tone: 'neutral' | 'success' | 'danger' | 'warning' | 'info' }> = {
    new: { label: 'neu', tone: 'info' },
    approved: { label: 'freigegeben', tone: 'success' },
    blocked: { label: 'gesperrt', tone: 'danger' },
    pending: { label: 'wartet auf Freigabe', tone: 'warning' },
  };
  const STATUS_FILTER = [
    { value: '', label: 'Alle Status' },
    { value: 'new', label: 'neu' },
    { value: 'approved', label: 'freigegeben' },
    { value: 'blocked', label: 'gesperrt' },
    { value: 'pending', label: 'wartet auf Freigabe' },
    { value: 'changed', label: 'geändert' },
    { value: 'suspicious', label: 'verdächtig' },
  ];
  const ts = (s: string | null | undefined) => (s ? dateTime(Date.parse(s)) : '–');

  // ------------------------------------------------------------ Detail
  let detailOpen = $state(false);
  let detail = $state<S['ToolDetail'] | null>(null);
  async function openDetail(t: ToolItem) {
    detail = null;
    detailOpen = true;
    try {
      detail = await tools.get(t.id);
    } catch (e) {
      detailOpen = false;
      handleError(e, { action: 'Tool laden' });
    }
  }

  // ------------------------------------------------------------ Freigeben/Sperren
  type Level = 'global' | 'organization' | 'team' | 'api-key';
  let dOpen = $state(false);
  let dTool = $state<ToolItem | null>(null);
  let dApprove = $state(true);
  let dLevel = $state<Level>('global');
  let dReject = $state(false);
  let dBusy = $state(false);
  let dError = $state<string | null>(null);
  function openDecision(t: ToolItem, approve: boolean) {
    dTool = t;
    dApprove = approve;
    dLevel = 'global';
    dReject = false;
    dError = null;
    dOpen = true;
  }
  const levelItems = (t: ToolItem | null) => [
    { value: 'global', label: 'Global (alle)' },
    ...(t?.organization ? [{ value: 'organization', label: `Organisation ${t.organization}` }] : []),
    ...(t?.team ? [{ value: 'team', label: `Team ${t.team}` }] : []),
    ...(t?.apiKey ? [{ value: 'api-key', label: `API-Key ${t.apiKey}` }] : []),
  ];
  async function submitDecision() {
    if (!dTool) return;
    dBusy = true;
    dError = null;
    const body: S['ToolDecisionInput'] = { scope: { level: dLevel } };
    if (!dApprove) body.action = dReject ? 'block-reject' : 'block-strip';
    try {
      const r = dApprove ? await tools.approve(dTool.id, body) : await tools.block(dTool.id, body);
      toast('success', dApprove ? 'Tool freigegeben' : 'Tool gesperrt', `${dTool.name} · Richtlinie ${r.policy}`);
      dOpen = false;
      void list.reload();
      void policies.reload();
    } catch (e) {
      dError = formErrorOf(e, dApprove ? 'Tool freigeben' : 'Tool sperren');
    } finally {
      dBusy = false;
    }
  }
  async function acknowledge(t: ToolItem) {
    try {
      await tools.acknowledge(t.id);
      toast('success', 'Änderung bestätigt', t.name);
      void list.reload();
      if (detail?.tool.id === t.id) detail = await tools.get(t.id);
    } catch (e) {
      handleError(e, { action: 'Änderung bestätigen' });
    }
  }
  const writable = (t: ToolItem) =>
    t.team ? canWrite({ kind: 'in-team', organization: t.organization ?? orgOfTeam(t.team) ?? '', team: t.team }) : t.organization ? canWrite({ kind: 'in-organization', organization: t.organization }) : canWrite({ kind: 'global' });

  // ------------------------------------------------------------ Richtlinien
  let pAction = $state('');
  const policies = new Paged<ToolPolicy>((q, l, c) => listPage(toolPolicies.path, q, l, c), () => ({ action: pAction || undefined }));
  let pFirst = true;
  $effect(() => {
    void pAction;
    if (pFirst) {
      pFirst = false;
      return;
    }
    untrack(() => void policies.first());
  });
  const ACTIONS: { value: S['ToolAction']; label: string }[] = [
    { value: 'allow', label: 'erlauben (allow)' },
    { value: 'block-strip', label: 'sperren – entfernen (block-strip)' },
    { value: 'block-reject', label: 'sperren – ablehnen (block-reject)' },
    { value: 'observe', label: 'beobachten (observe)' },
    { value: 'require-approval', label: 'erst nach Freigabe (require-approval)' },
  ];
  const ACTION_TONE: Record<S['ToolAction'], 'success' | 'danger' | 'info' | 'warning'> = {
    allow: 'success',
    'block-strip': 'danger',
    'block-reject': 'danger',
    observe: 'info',
    'require-approval': 'warning',
  };
  const LEVELS: { value: S['ScopeLevel']; label: string }[] = [
    { value: 'global', label: 'Global' },
    { value: 'organization', label: 'Organisation' },
    { value: 'team', label: 'Team' },
    { value: 'api-key', label: 'API-Key' },
    { value: 'model-group', label: 'Modellgruppe' },
  ];
  const blank = () => ({
    name: '',
    level: 'global' as S['ScopeLevel'],
    target: '',
    action: 'block-strip' as S['ToolAction'],
    unapproved: 'strip' as S['UnapprovedAction'],
    tools: '',
    servers: '',
    locked: false,
    enabled: true,
    description: '',
  });
  let pf = $state(blank());
  let pOpen = $state(false);
  let pEdit = $state<string | null>(null);
  let pEtag = $state<string | null>(null);
  let pBusy = $state(false);
  let pError = $state<string | null>(null);
  async function openPolicy(name: string | null) {
    pEdit = name;
    pEtag = null;
    pError = null;
    pf = blank();
    pOpen = true;
    if (!name) return;
    try {
      const { data, etag } = await toolPolicies.get(name);
      pEtag = etag;
      const s = data.spec;
      pf = {
        name,
        level: s.scope.level,
        target: s.scope.target ?? '',
        action: s.action,
        unapproved: s.unapproved ?? 'strip',
        tools: (s.tools ?? []).join('\n'),
        servers: (s.servers ?? []).join('\n'),
        locked: !!s.locked,
        enabled: s.enabled ?? true,
        description: s.description ?? '',
      };
    } catch (e) {
      pOpen = false;
      handleError(e, { action: 'Richtlinie laden' });
    }
  }
  async function submitPolicy() {
    pBusy = true;
    pError = null;
    const spec: S['ToolPolicySpec'] = {
      scope: pf.level === 'global' ? { level: 'global' } : { level: pf.level, target: pf.target.trim() },
      action: pf.action,
      tools: splitList(pf.tools),
      servers: splitList(pf.servers),
      enabled: pf.enabled,
      ...(pf.action === 'require-approval' ? { unapproved: pf.unapproved } : {}),
      ...(pf.locked && pf.action.startsWith('block') ? { locked: true } : {}),
      ...(pf.description.trim() ? { description: pf.description.trim() } : {}),
    };
    const name = pEdit ?? pf.name.trim();
    try {
      if (pEdit) await toolPolicies.update(pEdit, { metadata: { name }, spec }, pEtag);
      else await toolPolicies.create({ metadata: { name }, spec });
      toast('success', 'Richtlinie gespeichert', name);
      pOpen = false;
      void policies.reload();
      void list.reload();
    } catch (e) {
      pError = formErrorOf(e, 'Richtlinie speichern', () => pEdit && openPolicy(pEdit));
    } finally {
      pBusy = false;
    }
  }
  let delOpen = $state(false);
  let delName = $state('');
  async function runDelete(): Promise<string | null> {
    try {
      await toolPolicies.remove(delName);
      toast('success', 'Richtlinie gelöscht', delName);
      void policies.reload();
      void list.reload();
      return null;
    } catch (e) {
      return formErrorOf(e, 'Richtlinie löschen');
    }
  }
  const policyWritable = (p: ToolPolicy) => {
    const s = p.spec.scope;
    if (s.level === 'organization') return canWrite({ kind: 'in-organization', organization: s.target ?? '' });
    if (s.level === 'team') return canWrite({ kind: 'in-team', organization: orgOfTeam(s.target) ?? '', team: s.target ?? '' });
    if (s.level === 'api-key') return canWriteSomething();
    return canWrite({ kind: 'catalog' });
  };

  const columns: Column<ToolItem>[] = [
    { id: 'name', label: 'Tool', cell: nameCell },
    { id: 'status', label: 'Status', cell: statusCell },
    { id: 'who', label: 'Client · Team/Key', cell: whoCell, from: 'md' },
    { id: 'usage', label: 'Nutzung', cell: usageCell, from: 'lg', align: 'right' },
    { id: 'seen', label: 'Zuletzt', cell: seenCell, from: 'lg' },
    { id: 'actions', label: 'Aktionen', cell: actions, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
  const policyColumns: Column<ToolPolicy>[] = [
    { id: 'name', label: 'Richtlinie', cell: pNameCell },
    { id: 'action', label: 'Aktion', cell: pActionCell },
    { id: 'scope', label: 'Ebene', cell: pScopeCell, from: 'md' },
    { id: 'tools', label: 'Muster', cell: pToolsCell, from: 'md' },
    { id: 'actions', label: 'Aktionen', cell: pActions, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
</script>

{#snippet nameCell(t: ToolItem)}
  <button type="button" class="block text-left" onclick={() => openDetail(t)} data-testid="tool-name">
    <span class="flex items-center gap-1.5 font-mono text-sm font-medium hover:underline">{t.name}</span>
    <span class="mt-0.5 flex flex-wrap items-center gap-1">
      <Badge mono tone="outline">{t.kind}</Badge>
      <Badge mono tone={t.origin === 'client' ? 'neutral' : 'accent'}>{t.origin === 'client' ? 'Client' : `MCP ${t.origin}`}</Badge>
      {#if t.gatewayMcp}<Badge tone="info">Gateway-MCP</Badge>{/if}
      {#if t.changed}<Badge tone="warning" icon="alert">geändert</Badge>{/if}
      {#if t.suspicious.length}<Badge tone="danger" icon="alert">verdächtig</Badge>{/if}
    </span>
  </button>
{/snippet}
{#snippet statusCell(t: ToolItem)}
  <span class="flex flex-col gap-0.5">
    <Badge tone={STATUS[t.status].tone} dot>{STATUS[t.status].label}</Badge>
    {#if t.policy}<span class="font-mono text-xs text-fg-muted">{t.policy}</span>{/if}
  </span>
{/snippet}
{#snippet whoCell(t: ToolItem)}
  <span class="block max-w-64 truncate text-sm" title={t.client ?? ''}>{t.client ?? '–'}</span>
  <span class="block font-mono text-xs text-fg-muted">{t.team ?? t.organization ?? 'ohne Team'}{t.apiKey ? ` · ${t.apiKey}` : ''}</span>
{/snippet}
{#snippet usageCell(t: ToolItem)}
  <span class="num block text-sm">{num(t.requests)} Anfr. · {num(t.calls)} Aufrufe</span>
  {#if t.blocked}<span class="num block text-xs text-danger-fg">{num(t.blocked)} blockiert</span>{/if}
{/snippet}
{#snippet seenCell(t: ToolItem)}<span class="num text-sm text-fg-muted" title="zuerst {ts(t.firstSeen)}">{ts(t.lastSeen)}</span>{/snippet}
{#snippet actions(t: ToolItem)}
  <DropdownMenu.Root>
    <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {t.name}"><Icon name="more" /></DropdownMenu.Trigger>
    <DropdownMenu.Portal>
      <DropdownMenu.Content class={cx(popover, 'w-52')} align="end" sideOffset={4}>
        <DropdownMenu.Item class={menuItem} onSelect={() => openDetail(t)}><Icon name="eye" size={14} /> Details</DropdownMenu.Item>
        {#if canWriteSomething()}
          <DropdownMenu.Separator class={menuSeparator} />
          <DropdownMenu.Item class={menuItem} onSelect={() => openDecision(t, true)}><Icon name="check-circle" size={14} /> Freigeben …</DropdownMenu.Item>
          <DropdownMenu.Item class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')} onSelect={() => openDecision(t, false)}><Icon name="ban" size={14} /> Sperren …</DropdownMenu.Item>
          {#if t.changed && writable(t)}<DropdownMenu.Item class={menuItem} onSelect={() => acknowledge(t)}><Icon name="check" size={14} /> Änderung bestätigen</DropdownMenu.Item>{/if}
        {/if}
      </DropdownMenu.Content>
    </DropdownMenu.Portal>
  </DropdownMenu.Root>
{/snippet}
{#snippet pNameCell(p: ToolPolicy)}
  <span class="block font-mono text-sm font-medium">{p.metadata.name}</span>
  {#if p.spec.description}<span class="block truncate text-xs text-fg-muted">{p.spec.description}</span>{/if}
{/snippet}
{#snippet pActionCell(p: ToolPolicy)}
  <span class="flex flex-wrap items-center gap-1">
    <Badge tone={ACTION_TONE[p.spec.action]}>{p.spec.action}</Badge>
    {#if p.spec.locked}<Badge tone="outline" icon="lock">gesperrt</Badge>{/if}
    {#if p.spec.enabled === false}<Badge tone="neutral">aus</Badge>{/if}
  </span>
{/snippet}
{#snippet pScopeCell(p: ToolPolicy)}<span class="text-sm">{p.spec.scope.level}{p.spec.scope.target ? ` · ` : ''}<span class="font-mono">{p.spec.scope.target ?? ''}</span></span>{/snippet}
{#snippet pToolsCell(p: ToolPolicy)}
  <span class="flex flex-wrap gap-1">
    {#each p.spec.tools ?? [] as t (t)}<Badge tone={t.startsWith('!') ? 'danger' : 'outline'} mono>{t}</Badge>{:else}<Badge tone="info">alle Tools</Badge>{/each}
    {#each p.spec.servers ?? [] as s (s)}<Badge tone="accent" mono>Server {s}</Badge>{/each}
  </span>
{/snippet}
{#snippet pActions(p: ToolPolicy)}
  {#if policyWritable(p)}
    <DropdownMenu.Root>
      <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {p.metadata.name}"><Icon name="more" /></DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content class={cx(popover, 'w-48')} align="end" sideOffset={4}>
          <DropdownMenu.Item class={menuItem} onSelect={() => openPolicy(p.metadata.name)}><Icon name="pencil" size={14} /> Bearbeiten</DropdownMenu.Item>
          <DropdownMenu.Separator class={menuSeparator} />
          <DropdownMenu.Item
            class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')}
            onSelect={() => {
              delName = p.metadata.name;
              delOpen = true;
            }}><Icon name="trash" size={14} /> Löschen …</DropdownMenu.Item
          >
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  {/if}
{/snippet}

<PageHeader title="Tools" description="Tools, die Anwendungen in Anfragen mitschicken: Herkunft, Nutzung, Änderungen der Beschreibung; Freigeben, Sperren und Richtlinien je Ebene.">
  {#snippet actions()}
    <Button icon="refresh" onclick={() => (tab === 'inventory' ? list.reload() : policies.reload())} loading={list.loading || policies.loading}>Aktualisieren</Button>
    {#if tab === 'policies' && canWriteSomething()}<Button variant="primary" icon="plus" onclick={() => openPolicy(null)} data-testid="create-tool-policy">Richtlinie anlegen</Button>{/if}
  {/snippet}
</PageHeader>

<Tabs.Root bind:value={tab}>
  <Tabs.List class={cx(tabsList, 'mb-4')}>
    <Tabs.Trigger value="inventory" class={tabsTrigger} data-testid="tab-inventory">Inventar</Tabs.Trigger>
    <Tabs.Trigger value="policies" class={tabsTrigger} data-testid="tab-policies">Richtlinien</Tabs.Trigger>
  </Tabs.List>
  <Tabs.Content value="inventory">
    <Card flush>
      <AsyncState loader={list}>
        {#snippet children(rows)}
          <DataTable
            {rows}
            {columns}
            server={list}
            key={(t) => t.id}
            caption="Tool-Inventar"
            search={() => ''}
            searchPlaceholder="Tool-Name …"
            testid="tools-table"
            resetFilters={() => {
              status = '';
              origin = '';
              client = '';
              team = '';
            }}
          >
            {#snippet toolbar()}
              <div class="w-44"><Select size="sm" label="Status" bind:value={status} items={STATUS_FILTER} /></div>
              <div class="w-36"><Input bind:value={origin} placeholder="Herkunft" aria-label="Herkunft (client oder MCP-Server)" list="tool-origins" /></div>
              <div class="w-40"><Input bind:value={client} placeholder="Client" aria-label="Client" /></div>
              <div class="w-36"><Input bind:value={team} placeholder="Team" aria-label="Team" list="tool-teams" /></div>
            {/snippet}
            {#snippet empty()}<EmptyState icon="puzzle" title="Noch keine Tools gesehen" text="Sobald Anwendungen Anfragen mit Tools schicken, erscheinen sie hier (tools.inventory)." />{/snippet}
          </DataTable>
        {/snippet}
      </AsyncState>
    </Card>
    <datalist id="tool-origins"><option value="client"></option>{#each [...new Set((list.data ?? []).map((t) => t.origin))] as o (o)}<option value={o}></option>{/each}</datalist>
    <datalist id="tool-teams">{#each catalog.teams as t (t.metadata.name)}<option value={t.metadata.name}></option>{/each}</datalist>
  </Tabs.Content>
  <Tabs.Content value="policies">
    <Card flush>
      <AsyncState loader={policies}>
        {#snippet children(rows)}
          <DataTable rows={rows} columns={policyColumns} server={policies} key={(p) => p.metadata.name} caption="Tool-Richtlinien" search={() => ''} searchPlaceholder="Name …" testid="tool-policies-table" resetFilters={() => (pAction = '')}>
            {#snippet toolbar()}
              <div class="w-56"><Select size="sm" label="Aktion" bind:value={pAction} items={[{ value: '', label: 'Alle Aktionen' }, ...ACTIONS]} /></div>
            {/snippet}
            {#snippet empty()}<EmptyState icon="shield" title="Noch keine Tool-Richtlinien" text="Ohne Richtlinie sind alle Tools erlaubt. „Freigeben“/„Sperren“ im Inventar legt Richtlinien automatisch an." />{/snippet}
          </DataTable>
        {/snippet}
      </AsyncState>
    </Card>
  </Tabs.Content>
</Tabs.Root>

<Sheet bind:open={detailOpen} title={detail?.tool.name ?? 'Tool'} description="Beschreibung, Schema, Hash-Verlauf und passende Richtlinien" size="lg" testid="tool-detail">
  {#if detail}
    {@const t = detail.tool}
    <div class="flex flex-col gap-5 px-5 py-4">
      <div class="flex flex-wrap items-center gap-1.5">
        <Badge tone={STATUS[t.status].tone} dot>{STATUS[t.status].label}</Badge>
        {#if t.changed}<Badge tone="warning" icon="alert">geändert {ts(t.changedAt)}</Badge>{/if}
        {#if t.suspicious.length}<Badge tone="danger" icon="alert">verdächtig: {t.suspicious.join(', ')}</Badge>{/if}
        {#if t.gatewayMcp}<Badge tone="info">Gateway-MCP</Badge>{/if}
      </div>
      {#if t.changed && writable(t)}
        <div><Button size="sm" icon="check" onclick={() => acknowledge(t)} data-testid="tool-acknowledge">Änderung bestätigen</Button></div>
      {/if}
      <dl class="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
        <dt class="text-fg-muted">Art · Herkunft</dt><dd class="font-mono text-sm">{t.kind} · {t.origin}</dd>
        <dt class="text-fg-muted">Client</dt><dd class="text-sm break-all">{t.client ?? '–'}</dd>
        <dt class="text-fg-muted">Organisation / Team / Key</dt><dd class="font-mono text-sm">{t.organization ?? '–'} / {t.team ?? '–'} / {t.apiKey ?? '–'}</dd>
        <dt class="text-fg-muted">Anfragen</dt><dd class="num">{num(t.requests)}</dd>
        <dt class="text-fg-muted">Aufrufe durch das Modell</dt><dd class="num">{num(t.calls)}</dd>
        <dt class="text-fg-muted">Blockiert</dt><dd class="num">{num(t.blocked)}</dd>
        <dt class="text-fg-muted">Erste / letzte Sichtung</dt><dd class="num">{ts(t.firstSeen)} / {ts(t.lastSeen)}</dd>
        <dt class="text-fg-muted">Hash Beschreibung</dt><dd class="font-mono text-xs break-all">{t.descriptionHash}</dd>
        <dt class="text-fg-muted">Hash Schema</dt><dd class="font-mono text-xs break-all">{t.schemaHash}</dd>
      </dl>
      <section>
        <h3 class="mb-1 text-sm font-semibold">Beschreibung</h3>
        {#if detail.description}<p class="text-sm whitespace-pre-wrap" data-testid="tool-description">{detail.description}</p>
        {:else}<p class="text-sm text-fg-muted">Nicht gespeichert – nur bei payloadMode „masked“ oder „full“.</p>{/if}
      </section>
      {#if detail.parameters}
        <section>
          <h3 class="mb-1 text-sm font-semibold">Schema</h3>
          <CodeBlock code={JSON.stringify(detail.parameters, null, 2)} lang="json" />
        </section>
      {/if}
      <section>
        <h3 class="mb-1 text-sm font-semibold">Verlauf</h3>
        <ol class="flex flex-col gap-1.5" data-testid="tool-history">
          {#each detail.history as h, i (i)}
            <li class="rounded-md border border-line px-3 py-2 text-sm">
              <span class="flex items-center gap-2"><Badge tone={h.event === 'changed' ? 'warning' : 'info'}>{h.event === 'changed' ? 'geändert' : 'erstmals gesehen'}</Badge><span class="num text-fg-muted">{ts(h.seenAt)}</span></span>
              <span class="mt-1 block font-mono text-xs text-fg-muted">Beschr. {h.descriptionHash.slice(0, 12)}… · Schema {h.schemaHash.slice(0, 12)}…{h.client ? ` · ${h.client}` : ''}</span>
            </li>
          {/each}
        </ol>
      </section>
      <section>
        <h3 class="mb-1 text-sm font-semibold">Passende Richtlinien</h3>
        <span class="flex flex-wrap gap-1">
          {#each detail.policies as p (p.name)}<Badge tone={ACTION_TONE[p.action]} mono>{p.name} · {p.action} · {p.scope.level}{p.scope.target ? ` ${p.scope.target}` : ''}</Badge>{:else}<span class="text-sm text-fg-muted">keine</span>{/each}
        </span>
      </section>
    </div>
  {/if}
</Sheet>

<FormDialog bind:open={dOpen} title={dApprove ? `„${dTool?.name}“ freigeben` : `„${dTool?.name}“ sperren`} busy={dBusy} error={dError} submitLabel={dApprove ? 'Freigeben' : 'Sperren'} onsubmit={submitDecision} testid="tool-decision-form">
  <Field label="Ebene" hint="Die Entscheidung gilt für diese Ebene; spezifischere Ebenen gehen vor (außer gesperrten Richtlinien)." class="sm:col-span-2">
    {#snippet children({ id })}<Select {id} bind:value={dLevel} items={levelItems(dTool)} />{/snippet}
  </Field>
  {#if !dApprove}
    <Field label="Wirkung" class="sm:col-span-2">
      {#snippet children({ id })}<Select {id} value={dReject ? 'reject' : 'strip'} onchange={(v) => (dReject = v === 'reject')} items={[{ value: 'strip', label: 'Tool aus der Anfrage entfernen' }, { value: 'reject', label: 'Anfrage ablehnen (403)' }]} />{/snippet}
    </Field>
  {/if}
</FormDialog>

<FormDialog bind:open={pOpen} title={pEdit ? `Richtlinie „${pEdit}“ bearbeiten` : 'Tool-Richtlinie anlegen'} busy={pBusy} error={pError} submitLabel={pEdit ? 'Speichern' : 'Anlegen'} onsubmit={submitPolicy} testid="tool-policy-form">
  {#if !pEdit}
    <Field label="Name" required hint={NAME_HINT} class="sm:col-span-2">
      {#snippet children({ id, describedby })}<Input {id} bind:value={pf.name} aria-describedby={describedby} autocomplete="off" placeholder="block-shell" />{/snippet}
    </Field>
  {/if}
  <Field label="Aktion">
    {#snippet children({ id })}<Select {id} bind:value={pf.action} items={ACTIONS} />{/snippet}
  </Field>
  {#if pf.action === 'require-approval'}
    <Field label="Nicht freigegebene Tools">
      {#snippet children({ id })}<Select {id} bind:value={pf.unapproved} items={[{ value: 'strip', label: 'entfernen' }, { value: 'reject', label: 'Anfrage ablehnen' }]} />{/snippet}
    </Field>
  {/if}
  <Field label="Ebene">
    {#snippet children({ id })}<Select {id} bind:value={pf.level} items={LEVELS} />{/snippet}
  </Field>
  {#if pf.level !== 'global'}
    <Field label="Ziel" required hint="Name der Organisation, des Teams, Keys bzw. der Modellgruppe">
      {#snippet children({ id, describedby })}<Input {id} bind:value={pf.target} aria-describedby={describedby} autocomplete="off" />{/snippet}
    </Field>
  {/if}
  <Field label="Tool-Muster" optional hint="Glob je Zeile (* und ?), „!muster“ schließt aus; passt auf vollen Namen oder Namen ohne Server-Präfix; leer = alle" class="sm:col-span-2">
    {#snippet children({ id, describedby })}<textarea {id} class={[textarea, 'min-h-20 font-mono']} bind:value={pf.tools} aria-describedby={describedby} spellcheck="false" placeholder={'mcp__github__delete_*\n!*_readonly'}></textarea>{/snippet}
  </Field>
  <Field label="Herkunft (MCP-Server)" optional hint="Glob je Zeile; „client“ = vom Client definierte Tools; leer = jede" class="sm:col-span-2">
    {#snippet children({ id, describedby })}<textarea {id} class={[textarea, 'min-h-12 font-mono']} bind:value={pf.servers} aria-describedby={describedby} spellcheck="false" placeholder="github"></textarea>{/snippet}
  </Field>
  {#if pf.action.startsWith('block')}
    <Field label="Sperre" hint="gesperrt = gilt auch gegen spezifischere Freigaben">
      {#snippet children({ id })}<Select {id} value={pf.locked ? 'true' : 'false'} onchange={(v) => (pf.locked = v === 'true')} items={[{ value: 'false', label: 'überschreibbar' }, { value: 'true', label: 'gesperrt (locked)' }]} />{/snippet}
    </Field>
  {/if}
  <Field label="Aktiv">
    {#snippet children({ id })}<Select {id} value={pf.enabled ? 'true' : 'false'} onchange={(v) => (pf.enabled = v === 'true')} items={[{ value: 'true', label: 'an' }, { value: 'false', label: 'aus' }]} />{/snippet}
  </Field>
  <Field label="Beschreibung" optional class="sm:col-span-2">
    {#snippet children({ id })}<Input {id} bind:value={pf.description} autocomplete="off" />{/snippet}
  </Field>
</FormDialog>

<ConfirmDialog bind:open={delOpen} title="Richtlinie löschen?" confirmLabel="Löschen" onconfirm={runDelete}>
  <p>„<span class="font-mono text-fg">{delName}</span>“ wird gelöscht; ihre Wirkung endet mit dem nächsten Snapshot.</p>
</ConfirmDialog>
