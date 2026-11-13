<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Teams & Nutzer (AIGW-49, AIGW-77): Organisationen, Teams und Nutzer mit serverseitiger Pagination/Suche (D11).
  // Organisationen und Teams öffnen ihre Detailseite (D8: /organizations/{name}, /teams/{name}); Nutzer ein
  // Seitenpanel mit Einladung (F90). Schreibaktionen je Rolle (rbac.svelte.ts), Formular in EntityFormDialog.
  import { DropdownMenu, Tabs } from 'bits-ui';
  import { untrack } from 'svelte';
  import { listPage } from '../lib/api/client';
  import { organizations, teams, users, type Organization, type Team, type User } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import DetailSheet from '../lib/admin/DetailSheet.svelte';
  import { formErrorOf } from '../lib/admin/errors.svelte';
  import { Paged } from '../lib/admin/paged.svelte';
  import { catalog, loadCatalog } from '../lib/context.svelte';
  import { ago } from '../lib/format';
  import { detailPath } from '../lib/nav';
  import { canAdminWrite, canWrite } from '../lib/rbac.svelte';
  import { href, navigate, route } from '../lib/router.svelte';
  import { bindingLabel } from '../lib/session.svelte';
  import { toast } from '../lib/toast.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../lib/ui/DataTable.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Select from '../lib/ui/Select.svelte';
  import { btn, cx, menuItem, menuSeparator, popover, tabsList, tabsTrigger } from '../lib/ui/styles';
  import EntityFormDialog, { type Kind } from './teams/EntityFormDialog.svelte';
  import InvitationPanel from './teams/InvitationPanel.svelte';

  const TABS: { value: Kind; label: string }[] = [
    { value: 'org', label: 'Organisationen' },
    { value: 'team', label: 'Teams' },
    { value: 'user', label: 'Nutzer' },
  ];
  const initialTab = new URLSearchParams(route.search).get('tab');
  let tab = $state<Kind>(TABS.some((t) => t.value === initialTab) ? (initialTab as Kind) : 'team');
  let orgFilter = $state(new URLSearchParams(route.search).get('organization') ?? '');

  const orgs = new Paged<Organization>((q, l, c) => listPage(organizations.path, q, l, c));
  const teamList = new Paged<Team>((q, l, c) => listPage(teams.path, q, l, c), () => ({ organization: orgFilter || undefined }));
  // Nutzer brauchen eine globale Leserolle – erst beim Öffnen des Tabs laden (403 → Hinweis).
  let userList = $state<Paged<User> | null>(null);
  $effect(() => {
    if (tab === 'user' && !userList) userList = new Paged<User>((q, l, c) => listPage(users.path, q, l, c));
  });
  let first = true;
  $effect(() => {
    void orgFilter;
    if (first) {
      first = false;
      return;
    }
    untrack(() => void teamList.first());
  });

  // ------------------------------------------------------------ Formular
  let dlgOpen = $state(false);
  let kind = $state<Kind>('team');
  let editName = $state<string | null>(null);
  function openForm(k: Kind, name: string | null) {
    kind = k;
    editName = name;
    dlgOpen = true;
  }
  function reloadKind(k: Kind) {
    if (k === 'org') void orgs.reload();
    else if (k === 'team') void teamList.reload();
    else void userList?.reload();
    if (k !== 'user') void loadCatalog();
  }

  // ------------------------------------------------------------ Löschen, Nutzer-Detail
  const KIND_LABEL: Record<Kind, string> = { org: 'Organisation', team: 'Team', user: 'Nutzer' };
  const resOf = (k: Kind) => (k === 'org' ? organizations : k === 'team' ? teams : users);
  let del = $state<{ kind: Kind; name: string } | null>(null);
  let delOpen = $state(false);
  async function runDelete(): Promise<string | null> {
    if (!del) return null;
    const { kind: k, name } = del;
    try {
      const res = resOf(k);
      const { etag: tag } = await res.get(name);
      await res.remove(name, tag);
      toast('success', `${KIND_LABEL[k]} gelöscht`, name);
      reloadKind(k);
      return null;
    } catch (e) {
      return formErrorOf(e, `${KIND_LABEL[k]} löschen`, () => reloadKind(k));
    }
  }
  function askDelete(k: Kind, name: string) {
    del = { kind: k, name };
    delOpen = true;
  }
  let shown = $state<User | null>(null);
  let sheetOpen = $state(false);
  function showUser(u: User) {
    shown = u;
    sheetOpen = true;
  }

  // ------------------------------------------------------------ Rechte
  const writableOrgs = () => catalog.orgs.map((o) => o.metadata.name).filter((o) => canWrite({ kind: 'team', organization: o, team: '\u0000neu' }));
  const canEditOrg = (o: Organization) => canWrite({ kind: 'organization', organization: o.metadata.name });
  const canEditTeam = (t: Team) => canWrite({ kind: 'team', organization: t.spec.organization, team: t.metadata.name });
  const open = (k: Kind, name: string) => navigate(detailPath(k === 'org' ? 'organization' : 'team', name));

  const orgColumns: Column<Organization>[] = [
    { id: 'name', label: 'Organisation', cell: orgName },
    { id: 'updated', label: 'Geändert', cell: updated, from: 'md' },
    { id: 'actions', label: 'Aktionen', cell: orgActions, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
  const teamColumns: Column<Team>[] = [
    { id: 'name', label: 'Team', cell: teamName },
    { id: 'org', label: 'Organisation', cell: teamOrg, from: 'sm' },
    { id: 'models', label: 'Modelle', cell: teamModels, from: 'lg' },
    { id: 'updated', label: 'Geändert', cell: updated, from: 'md' },
    { id: 'actions', label: 'Aktionen', cell: teamActions, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
  const userColumns: Column<User>[] = [
    { id: 'name', label: 'Nutzer', cell: userName },
    { id: 'roles', label: 'Rollen', cell: userRoles },
    { id: 'oidc', label: 'OIDC', cell: userOidc, from: 'lg' },
    { id: 'actions', label: 'Aktionen', cell: userActions, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
</script>

{#snippet updated(o: Organization | Team)}<span class="text-sm text-fg-muted">{o.metadata.updatedAt ? ago(Date.parse(o.metadata.updatedAt)) : '–'}</span>{/snippet}
{#snippet orgName(o: Organization)}
  <a class="block font-medium text-fg hover:text-accent-text" href={href(detailPath('organization', o.metadata.name))}>{o.spec.displayName || o.metadata.name}</a>
  <span class="block font-mono text-xs text-fg-muted">{o.metadata.name}</span>
{/snippet}
{#snippet rowMenu(k: Kind, name: string, editable: boolean)}
  <DropdownMenu.Root>
    <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {name}"><Icon name="more" /></DropdownMenu.Trigger>
    <DropdownMenu.Portal>
      <DropdownMenu.Content class={cx(popover, 'w-44')} align="end" sideOffset={4}>
        {#if k !== 'user'}
          <DropdownMenu.Item class={menuItem} onSelect={() => open(k, name)}><Icon name="eye" size={14} /> Details</DropdownMenu.Item>
        {/if}
        {#if editable}
          <DropdownMenu.Item class={menuItem} onSelect={() => openForm(k, name)}><Icon name="pencil" size={14} /> Bearbeiten</DropdownMenu.Item>
        {/if}
        {#if k === 'team'}
          <DropdownMenu.Item class={menuItem} onSelect={() => navigate(`/keys?team=${encodeURIComponent(name)}`)}><Icon name="key" size={14} /> Keys anzeigen</DropdownMenu.Item>
        {/if}
        <DropdownMenu.Item class={menuItem} onSelect={() => navigator.clipboard?.writeText(name)}><Icon name="copy" size={14} /> Name kopieren</DropdownMenu.Item>
        {#if editable}
          <DropdownMenu.Separator class={menuSeparator} />
          <DropdownMenu.Item class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')} onSelect={() => askDelete(k, name)}><Icon name="trash" size={14} /> Löschen …</DropdownMenu.Item>
        {/if}
      </DropdownMenu.Content>
    </DropdownMenu.Portal>
  </DropdownMenu.Root>
{/snippet}
{#snippet orgActions(o: Organization)}{@render rowMenu('org', o.metadata.name, canEditOrg(o))}{/snippet}
{#snippet teamName(t: Team)}
  <a class="block font-medium text-fg hover:text-accent-text" href={href(detailPath('team', t.metadata.name))}>{t.spec.displayName || t.metadata.name}</a>
  <span class="block font-mono text-xs text-fg-muted">{t.metadata.name}</span>
{/snippet}
{#snippet teamOrg(t: Team)}<span class="font-mono text-sm text-fg-muted">{t.spec.organization}</span>{/snippet}
{#snippet teamModels(t: Team)}
  {#if !t.spec.allowedModels?.length}<Badge tone="outline">Alle Modelle</Badge>
  {:else}<span class="flex flex-wrap gap-1">{#each t.spec.allowedModels.slice(0, 3) as m (m)}<Badge mono>{m}</Badge>{/each}{#if t.spec.allowedModels.length > 3}<Badge tone="outline">+{t.spec.allowedModels.length - 3}</Badge>{/if}</span>{/if}
{/snippet}
{#snippet teamActions(t: Team)}{@render rowMenu('team', t.metadata.name, canEditTeam(t))}{/snippet}
{#snippet userName(u: User)}
  <span class="block font-medium text-fg">{u.spec.displayName || u.metadata.name}</span>
  <span class="block text-xs text-fg-muted">{u.spec.email ?? u.metadata.name}</span>
{/snippet}
{#snippet userRoles(u: User)}
  <span class="flex flex-wrap gap-1">{#each u.spec.roles ?? [] as r, i (i)}<Badge tone={r.role === 'admin' && !r.organization && !r.team ? 'accent' : 'neutral'}>{bindingLabel(r)}</Badge>{:else}<span class="text-sm text-fg-muted">nur über IdP-Gruppen</span>{/each}</span>
{/snippet}
{#snippet userOidc(u: User)}
  {#if u.spec.oidcSubject}<Badge tone="success" dot>verknüpft</Badge>{:else}<Badge tone="neutral" dot>noch nicht angemeldet</Badge>{/if}
{/snippet}
{#snippet userActions(u: User)}{@render rowMenu('user', u.metadata.name, canAdminWrite())}{/snippet}

<PageHeader title="Teams & Nutzer" description="Organisationen sind die oberste Mandantenebene; Teams bündeln Keys, Modellfreigaben und Budgets. Nutzer melden sich per OIDC an und erhalten Rollen direkt oder über IdP-Gruppen.">
  {#snippet actions()}
    {#if tab === 'org' && canAdminWrite()}<Button variant="primary" icon="plus" onclick={() => openForm('org', null)} data-testid="create-org">Organisation anlegen</Button>{/if}
    {#if tab === 'team' && writableOrgs().length}<Button variant="primary" icon="plus" onclick={() => openForm('team', null)} data-testid="create-team">Team anlegen</Button>{/if}
    {#if tab === 'user' && canAdminWrite()}<Button variant="primary" icon="plus" onclick={() => openForm('user', null)} data-testid="create-user">Nutzer anlegen</Button>{/if}
  {/snippet}
</PageHeader>

<Tabs.Root bind:value={tab} onValueChange={(v) => navigate(`/teams?tab=${v}`, true)}>
  <Tabs.List class={cx(tabsList, 'mb-4')}>
    {#each TABS as t (t.value)}
      <Tabs.Trigger value={t.value} class={tabsTrigger}>{t.label}</Tabs.Trigger>
    {/each}
  </Tabs.List>

  <Tabs.Content value="org">
    <Card flush>
      <AsyncState loader={orgs}>
        {#snippet children(rows)}
          <DataTable {rows} columns={orgColumns} server={orgs} key={(o) => o.metadata.name} caption="Organisationen" search={() => ''} searchPlaceholder="Name …" testid="orgs-table" onrowclick={(o) => open('org', o.metadata.name)}>
            {#snippet empty()}<EmptyState icon="building" title="Noch keine Organisationen" text="Organisationen gruppieren Teams; Rollen lassen sich auf eine Organisation beschränken." />{/snippet}
          </DataTable>
        {/snippet}
      </AsyncState>
    </Card>
  </Tabs.Content>

  <Tabs.Content value="team">
    <Card flush>
      <AsyncState loader={teamList}>
        {#snippet children(rows)}
          <DataTable
            {rows}
            columns={teamColumns}
            server={teamList}
            filtered={!!orgFilter}
            key={(t) => t.metadata.name}
            caption="Teams"
            search={() => ''}
            searchPlaceholder="Name …"
            testid="teams-table"
            onrowclick={(t) => open('team', t.metadata.name)}
            resetFilters={() => (orgFilter = '')}
          >
            {#snippet toolbar()}
              <div class="w-48">
                <Select size="sm" label="Organisation" icon="building" bind:value={orgFilter} items={[{ value: '', label: 'Alle Organisationen' }, ...catalog.orgs.map((o) => ({ value: o.metadata.name, label: o.spec.displayName || o.metadata.name }))]} />
              </div>
            {/snippet}
            {#snippet empty()}<EmptyState icon="users" title="Noch keine Teams" text="Teams gehören zu einer Organisation und bündeln Keys." />{/snippet}
          </DataTable>
        {/snippet}
      </AsyncState>
    </Card>
  </Tabs.Content>

  <Tabs.Content value="user">
    <Card flush>
      {#if userList}
        {@const ul = userList}
        <AsyncState loader={ul}>
          {#snippet children(rows)}
            <DataTable {rows} columns={userColumns} server={ul} key={(u) => u.metadata.name} caption="Nutzer" search={() => ''} searchPlaceholder="Name …" testid="users-table" onrowclick={showUser}>
              {#snippet empty()}<EmptyState icon="user" title="Noch keine Nutzer" text="Nutzer entstehen beim ersten OIDC-Login (provision_users) oder werden vorab mit OIDC-Kennung angelegt und eingeladen." />{/snippet}
            </DataTable>
          {/snippet}
        </AsyncState>
      {/if}
    </Card>
  </Tabs.Content>
</Tabs.Root>

<EntityFormDialog bind:open={dlgOpen} {kind} {editName} onsaved={() => reloadKind(kind)} />

<ConfirmDialog bind:open={delOpen} title="{del ? KIND_LABEL[del.kind] : ''} löschen?" confirmLabel="Löschen" onconfirm={runDelete}>
  {#if del?.kind === 'org'}
    <p>„<span class="font-mono text-fg">{del.name}</span>“ wird gelöscht. Nur möglich, wenn keine Teams mehr dazugehören.</p>
  {:else if del?.kind === 'team'}
    <p>„<span class="font-mono text-fg">{del.name}</span>“ wird gelöscht. Nur möglich, wenn das Team keine Keys mehr hat.</p>
  {:else if del}
    <p>„<span class="font-mono text-fg">{del.name}</span>“ wird gelöscht; laufende Sessions des Nutzers enden sofort.</p>
  {/if}
</ConfirmDialog>

<DetailSheet bind:open={sheetOpen} title={shown ? `Nutzer ${shown.metadata.name}` : ''} object={shown}>
  {#if shown}
    {#key shown.metadata.name}<InvitationPanel user={shown.metadata.name} linked={!!shown.spec.oidcSubject} />{/key}
  {/if}
  {#snippet footer()}
    {#if shown && canAdminWrite()}
      {@const u = shown}
      <Button
        icon="pencil"
        onclick={() => {
          sheetOpen = false;
          openForm('user', u.metadata.name);
        }}>Bearbeiten</Button
      >
    {/if}
  {/snippet}
</DetailSheet>
