<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Teams & Nutzer (AIGW-49): Organisationen, Teams und Nutzer – Liste, Detail (Seitenpanel), Anlegen, Bearbeiten
  // (ETag/If-Match), Löschen. Schreibaktionen je Rolle: Organisation anlegen und Nutzer nur globaler Admin, Organisation
  // ändern Admin der Organisation, Teams Admin/Org-Admin (rbac.svelte.ts). Budgets/Limits folgen mit AIGW-30/31.
  import { DropdownMenu, Tabs } from 'bits-ui';
  import { organizations, teams, users, NAME_HINT, NAME_RE, type Organization, type RoleBinding, type Team, type User } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import DetailSheet from '../lib/admin/DetailSheet.svelte';
  import { formErrorOf, handleError, Loader, splitList } from '../lib/admin/errors.svelte';
  import FormDialog from '../lib/admin/FormDialog.svelte';
  import RoleEditor from '../lib/admin/RoleEditor.svelte';
  import { loadCatalog } from '../lib/context.svelte';
  import { ago } from '../lib/format';
  import { canWrite, isGlobalAdmin } from '../lib/rbac.svelte';
  import { navigate, route } from '../lib/router.svelte';
  import { bindingLabel } from '../lib/session.svelte';
  import { toast } from '../lib/toast.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../lib/ui/DataTable.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import Field from '../lib/ui/Field.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import Input from '../lib/ui/Input.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Select from '../lib/ui/Select.svelte';
  import { btn, cx, menuItem, menuSeparator, popover, tabsList, tabsTrigger } from '../lib/ui/styles';

  type Kind = 'org' | 'team' | 'user';
  const TABS: { value: Kind; label: string }[] = [
    { value: 'org', label: 'Organisationen' },
    { value: 'team', label: 'Teams' },
    { value: 'user', label: 'Nutzer' },
  ];
  const initialTab = new URLSearchParams(route.search).get('tab');
  let tab = $state<Kind>(TABS.some((t) => t.value === initialTab) ? (initialTab as Kind) : 'team');

  const orgs = new Loader(() => organizations.list());
  const teamList = new Loader(() => teams.list());
  // Nutzer brauchen eine globale Leserolle – erst beim Öffnen des Tabs laden (403 → Hinweis).
  let userList = $state<Loader<User[]> | null>(null);
  $effect(() => {
    if (tab === 'user' && !userList) userList = new Loader(() => users.list());
  });

  let q = $state({ org: '', team: '', user: '' });
  let orgFilter = $state('');

  // ------------------------------------------------------------ Formular (ein Dialog für alle drei Arten)
  let dlgOpen = $state(false);
  let kind = $state<Kind>('team');
  let editName = $state<string | null>(null);
  let etag = $state<string | null>(null);
  let loadedSpec = $state<Record<string, unknown>>({});
  let labels = $state<Record<string, string>>({});
  let busy = $state(false);
  let loading = $state(false);
  let formError = $state<string | null>(null);
  let errors = $state<Record<string, string>>({});
  let f = $state({ name: '', displayName: '', organization: '', allowedModels: '', email: '', oidcIssuer: '', oidcSubject: '', roles: [] as RoleBinding[] });

  const resOf = (k: Kind) => (k === 'org' ? organizations : k === 'team' ? teams : users);
  const KIND_LABEL: Record<Kind, string> = { org: 'Organisation', team: 'Team', user: 'Nutzer' };

  async function openForm(k: Kind, name: string | null) {
    kind = k;
    editName = name;
    etag = null;
    loadedSpec = {};
    labels = {};
    formError = null;
    errors = {};
    f = { name: '', displayName: '', organization: k === 'team' ? (writableOrgs()[0] ?? '') : '', allowedModels: '', email: '', oidcIssuer: '', oidcSubject: '', roles: [] };
    dlgOpen = true;
    if (name) await load(k, name);
  }
  async function load(k: Kind, name: string) {
    loading = true;
    try {
      const r = await resOf(k).get(name);
      const o = r.data as Organization | Team | User;
      etag = r.etag;
      labels = o.metadata.labels ?? {};
      const s = o.spec as Record<string, unknown>;
      loadedSpec = s;
      f = {
        name,
        displayName: (s.displayName as string | null) ?? '',
        organization: (s.organization as string) ?? '',
        allowedModels: ((s.allowedModels as string[] | undefined) ?? []).join(', '),
        email: (s.email as string | null) ?? '',
        oidcIssuer: (s.oidcIssuer as string | null) ?? '',
        oidcSubject: (s.oidcSubject as string | null) ?? '',
        roles: [...((s.roles as RoleBinding[] | undefined) ?? [])],
      };
    } catch (e) {
      handleError(e, { action: `${KIND_LABEL[k]} laden` });
      dlgOpen = false;
    } finally {
      loading = false;
    }
  }

  async function submit() {
    const e: Record<string, string> = {};
    if (!editName && !NAME_RE.test(f.name)) e.name = 'Ungültiger Name: a-z, 0-9, „-“, „.“, „_“, beginnt alphanumerisch.';
    if (kind === 'team' && !f.organization) e.organization = 'Bitte eine Organisation wählen.';
    if (kind === 'user' && f.email && !/^[^@\s]+@[^@\s]+$/.test(f.email)) e.email = 'Keine gültige E-Mail-Adresse.';
    if (kind === 'user' && f.roles.some((r) => (r.role === 'team-admin' && !r.team) || (r.role === 'org-admin' && !r.organization && !r.team))) {
      e.roles = 'Org-Admin braucht eine Organisation, Team-Admin ein Team.';
    }
    errors = e;
    formError = null;
    if (Object.keys(e).length) return;
    const name = editName ?? f.name;
    const opt = (s: string) => s.trim() || null;
    const metadata = { name, labels };
    busy = true;
    try {
      if (kind === 'org') {
        const spec = { ...loadedSpec, displayName: opt(f.displayName) };
        if (editName) await organizations.update(name, { metadata, spec }, etag);
        else await organizations.create({ metadata, spec });
      } else if (kind === 'team') {
        const spec = { ...loadedSpec, organization: f.organization, displayName: opt(f.displayName), allowedModels: splitList(f.allowedModels) };
        if (editName) await teams.update(name, { metadata, spec }, etag);
        else await teams.create({ metadata, spec });
      } else {
        const spec = { ...loadedSpec, displayName: opt(f.displayName), email: opt(f.email), oidcIssuer: opt(f.oidcIssuer), oidcSubject: opt(f.oidcSubject), roles: f.roles };
        if (editName) await users.update(name, { metadata, spec }, etag);
        else await users.create({ metadata, spec });
      }
      dlgOpen = false;
      toast('success', `${KIND_LABEL[kind]} gespeichert`, name);
      reloadKind(kind);
    } catch (err) {
      handleError(err, { action: `${KIND_LABEL[kind]} speichern`, form: (m) => (formError = m), reload: () => editName && load(kind, editName) });
    } finally {
      busy = false;
    }
  }

  function reloadKind(k: Kind) {
    if (k === 'org') void orgs.reload();
    else if (k === 'team') void teamList.reload();
    else void userList?.reload();
    if (k !== 'user') void loadCatalog();
  }

  // ------------------------------------------------------------ Löschen, Detail
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
  let detail = $state<{ kind: Kind; obj: Organization | Team | User } | null>(null);
  let detailOpen = $state(false);
  function show(k: Kind, obj: Organization | Team | User) {
    detail = { kind: k, obj };
    detailOpen = true;
  }

  // ------------------------------------------------------------ Rechte
  const writableOrgs = () => (orgs.data ?? []).map((o) => o.metadata.name).filter((o) => canWrite({ kind: 'team', organization: o, team: '\u0000neu' }));
  const canEditOrg = (o: Organization) => canWrite({ kind: 'organization', organization: o.metadata.name });
  const canEditTeam = (t: Team) => canWrite({ kind: 'team', organization: t.spec.organization, team: t.metadata.name });
  const teamsOf = (o: string) => (teamList.data ?? []).filter((t) => t.spec.organization === o).length;

  const orgColumns: Column<Organization>[] = [
    { id: 'name', label: 'Organisation', cell: orgName, sort: (o) => o.metadata.name },
    { id: 'teams', label: 'Teams', cell: orgTeams, sort: (o) => teamsOf(o.metadata.name), align: 'right' },
    { id: 'updated', label: 'Geändert', cell: updated, sort: (o) => o.metadata.updatedAt ?? '', from: 'md' },
    { id: 'actions', label: 'Aktionen', cell: orgActions, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
  const teamColumns: Column<Team>[] = [
    { id: 'name', label: 'Team', cell: teamName, sort: (t) => t.metadata.name },
    { id: 'org', label: 'Organisation', cell: teamOrg, sort: (t) => t.spec.organization, from: 'sm' },
    { id: 'models', label: 'Modelle', cell: teamModels, from: 'lg' },
    { id: 'updated', label: 'Geändert', cell: updated, sort: (t) => t.metadata.updatedAt ?? '', from: 'md' },
    { id: 'actions', label: 'Aktionen', cell: teamActions, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
  const userColumns: Column<User>[] = [
    { id: 'name', label: 'Nutzer', cell: userName, sort: (u) => u.metadata.name },
    { id: 'roles', label: 'Rollen', cell: userRoles },
    { id: 'oidc', label: 'OIDC', cell: userOidc, from: 'lg' },
    { id: 'actions', label: 'Aktionen', cell: userActions, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
</script>

{#snippet updated(o: Organization | Team)}<span class="text-sm text-fg-muted">{o.metadata.updatedAt ? ago(Date.parse(o.metadata.updatedAt)) : '–'}</span>{/snippet}
{#snippet orgName(o: Organization)}
  <span class="block font-medium text-fg">{o.spec.displayName || o.metadata.name}</span>
  <span class="block font-mono text-xs text-fg-muted">{o.metadata.name}</span>
{/snippet}
{#snippet orgTeams(o: Organization)}<span class="num text-sm">{teamsOf(o.metadata.name)}</span>{/snippet}
{#snippet rowMenu(k: Kind, name: string, editable: boolean)}
  <DropdownMenu.Root>
    <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {name}"><Icon name="more" /></DropdownMenu.Trigger>
    <DropdownMenu.Portal>
      <DropdownMenu.Content class={cx(popover, 'w-44')} align="end" sideOffset={4}>
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
  <span class="block font-medium text-fg">{t.spec.displayName || t.metadata.name}</span>
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
{#snippet userActions(u: User)}{@render rowMenu('user', u.metadata.name, isGlobalAdmin())}{/snippet}

<PageHeader title="Teams & Nutzer" description="Organisationen sind die oberste Mandantenebene; Teams bündeln Keys, Modellfreigaben und Budgets. Nutzer melden sich per OIDC an und erhalten Rollen direkt oder über IdP-Gruppen.">
  {#snippet actions()}
    {#if tab === 'org' && isGlobalAdmin()}<Button variant="primary" icon="plus" onclick={() => openForm('org', null)} data-testid="create-org">Organisation anlegen</Button>{/if}
    {#if tab === 'team' && writableOrgs().length}<Button variant="primary" icon="plus" onclick={() => openForm('team', null)} data-testid="create-team">Team anlegen</Button>{/if}
    {#if tab === 'user' && isGlobalAdmin()}<Button variant="primary" icon="plus" onclick={() => openForm('user', null)} data-testid="create-user">Nutzer anlegen</Button>{/if}
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
            <DataTable
              {rows}
              columns={orgColumns}
              bind:q={q.org}
              key={(o) => o.metadata.name}
              caption="Organisationen"
              search={(o) => `${o.metadata.name} ${o.spec.displayName ?? ''}`}
              sortId="name"
              testid="orgs-table"
              onrowclick={(o) => show('org', o)}
            >
              {#snippet empty()}<EmptyState icon="building" title="Noch keine Organisationen" text="Organisationen gruppieren Teams; Rollen lassen sich auf eine Organisation beschränken." />{/snippet}
            </DataTable>
          {/snippet}
        </AsyncState>
      </Card>
    </Tabs.Content>

    <Tabs.Content value="team">
      <Card flush>
        <AsyncState loader={teamList}>
          {#snippet children(all)}
            <DataTable
              rows={all.filter((t) => !orgFilter || t.spec.organization === orgFilter)}
              columns={teamColumns}
              bind:q={q.team}
              key={(t) => t.metadata.name}
              caption="Teams"
              search={(t) => `${t.metadata.name} ${t.spec.displayName ?? ''} ${t.spec.organization}`}
              sortId="name"
              testid="teams-table"
              onrowclick={(t) => show('team', t)}
              resetFilters={() => (orgFilter = '')}
            >
              {#snippet toolbar()}
                <div class="w-48">
                  <Select size="sm" label="Organisation" icon="building" bind:value={orgFilter} items={[{ value: '', label: 'Alle Organisationen' }, ...(orgs.data ?? []).map((o) => ({ value: o.metadata.name, label: o.spec.displayName || o.metadata.name }))]} />
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
          <AsyncState loader={userList}>
            {#snippet children(rows)}
              <DataTable
                {rows}
                columns={userColumns}
                bind:q={q.user}
                key={(u) => u.metadata.name}
                caption="Nutzer"
                search={(u) => `${u.metadata.name} ${u.spec.displayName ?? ''} ${u.spec.email ?? ''}`}
                sortId="name"
                testid="users-table"
                onrowclick={(u) => show('user', u)}
              >
                {#snippet empty()}<EmptyState icon="user" title="Noch keine Nutzer" text="Nutzer entstehen beim ersten OIDC-Login (provision_users) oder werden vorab mit E-Mail angelegt (link_by_email)." />{/snippet}
              </DataTable>
            {/snippet}
          </AsyncState>
        {/if}
      </Card>
    </Tabs.Content>
  </Tabs.Root>

<FormDialog
  bind:open={dlgOpen}
  title={editName ? `${KIND_LABEL[kind]} „${editName}“ bearbeiten` : `${KIND_LABEL[kind]} anlegen`}
  size={kind === 'user' ? 'lg' : 'md'}
  {busy}
  {loading}
  error={formError}
  submitLabel={editName ? 'Speichern' : 'Anlegen'}
  onsubmit={submit}
  testid="entity-form"
>
  {#if !editName}
    <Field label="Name" required hint={NAME_HINT} error={errors.name} class={kind === 'org' ? 'sm:col-span-2' : ''}>
      {#snippet children({ id, describedby, invalid })}
        <Input {id} bind:value={f.name} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" placeholder={kind === 'org' ? 'acme' : kind === 'team' ? 'platform' : 'anna'} />
      {/snippet}
    </Field>
  {/if}
  {#if kind === 'team'}
    <Field label="Organisation" required error={errors.organization}>
      {#snippet children({ id, describedby, invalid })}
        <Select {id} {describedby} {invalid} bind:value={f.organization} items={(editName ? (orgs.data ?? []).map((o) => o.metadata.name) : writableOrgs()).map((o) => ({ value: o, label: o }))} />
      {/snippet}
    </Field>
  {/if}
  <Field label="Anzeigename" optional class="sm:col-span-2">
    {#snippet children({ id, describedby })}
      <Input {id} bind:value={f.displayName} aria-describedby={describedby} autocomplete="off" />
    {/snippet}
  </Field>
  {#if kind === 'team'}
    <Field label="Erlaubte Modellgruppen" optional hint="Namen oder Aliasse, mit Komma getrennt; leer = alle." class="sm:col-span-2">
      {#snippet children({ id, describedby })}
        <Input {id} bind:value={f.allowedModels} aria-describedby={describedby} autocomplete="off" placeholder="gpt-4o, claude-sonnet" />
      {/snippet}
    </Field>
  {/if}
  {#if kind === 'user'}
    <Field label="E-Mail" optional hint="Für die Verknüpfung beim ersten Login (bestätigte E-Mail beim IdP)." error={errors.email} class="sm:col-span-2">
      {#snippet children({ id, describedby, invalid })}
        <Input {id} type="email" bind:value={f.email} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" />
      {/snippet}
    </Field>
    <Field label="OIDC-Issuer" optional>
      {#snippet children({ id })}<Input {id} bind:value={f.oidcIssuer} autocomplete="off" placeholder="https://idp.example.org/realms/acme" />{/snippet}
    </Field>
    <Field label="OIDC-Subjekt (sub)" optional>
      {#snippet children({ id })}<Input {id} bind:value={f.oidcSubject} autocomplete="off" />{/snippet}
    </Field>
    <fieldset class="flex flex-col gap-1.5 sm:col-span-2">
      <legend class="mb-1 text-sm font-medium text-fg">Rollen <span class="text-xs font-normal text-fg-subtle">(zusätzlich zu Rollen aus IdP-Gruppen)</span></legend>
      <RoleEditor bind:value={f.roles} organizations={(orgs.data ?? []).map((o) => o.metadata.name)} teams={(teamList.data ?? []).map((t) => t.metadata.name)} />
      {#if errors.roles}<p class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{errors.roles}</p>{/if}
    </fieldset>
  {/if}
</FormDialog>

<ConfirmDialog bind:open={delOpen} title="{del ? KIND_LABEL[del.kind] : ''} löschen?" confirmLabel="Löschen" onconfirm={runDelete}>
  {#if del?.kind === 'org'}
    <p>„<span class="font-mono text-fg">{del.name}</span>“ wird gelöscht. Nur möglich, wenn keine Teams mehr dazugehören.</p>
  {:else if del?.kind === 'team'}
    <p>„<span class="font-mono text-fg">{del.name}</span>“ wird gelöscht. Nur möglich, wenn das Team keine Keys mehr hat.</p>
  {:else if del}
    <p>„<span class="font-mono text-fg">{del.name}</span>“ wird gelöscht; laufende Sessions des Nutzers enden sofort.</p>
  {/if}
</ConfirmDialog>

<DetailSheet bind:open={detailOpen} title={detail ? `${KIND_LABEL[detail.kind]} ${detail.obj.metadata.name}` : ''} object={detail?.obj ?? null}>
  {#snippet footer()}
    {#if detail && (detail.kind === 'org' ? canEditOrg(detail.obj as Organization) : detail.kind === 'team' ? canEditTeam(detail.obj as Team) : isGlobalAdmin())}
      {@const d = detail}
      <Button
        icon="pencil"
        onclick={() => {
          detailOpen = false;
          void openForm(d.kind, d.obj.metadata.name);
        }}>Bearbeiten</Button
      >
    {/if}
  {/snippet}
</DetailSheet>
