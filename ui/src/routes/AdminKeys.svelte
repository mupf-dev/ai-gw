<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Admin-API-Keys (AIGW-49, Admin-API §5.5): Zugänge für Automatisierung mit Rollen und Ablauf. Lesen: globale
  // Leserolle; anlegen/ändern/löschen nur globaler Admin. Klartext nur einmal nach dem Anlegen.
  import { DropdownMenu } from 'bits-ui';
  import { adminKeys, NAME_RE, type AdminApiKey, type RoleBinding } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf, fromLocalInput, handleError, Loader, toLocalInput } from '../lib/admin/errors.svelte';
  import FormDialog from '../lib/admin/FormDialog.svelte';
  import RoleEditor from '../lib/admin/RoleEditor.svelte';
  import SecretDialog from '../lib/admin/SecretDialog.svelte';
  import { catalog } from '../lib/context.svelte';
  import { ago, day } from '../lib/format';
  import { isGlobalAdmin } from '../lib/rbac.svelte';
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
  import { btn, cx, menuItem, menuSeparator, popover } from '../lib/ui/styles';

  const data = new Loader(() => adminKeys.list());
  let q = $state('');
  let formOpen = $state(false);
  let editName = $state<string | null>(null);
  let f = $state({ name: '', description: '', expiresAt: '', roles: [{ role: 'viewer' }] as RoleBinding[] });
  let errors = $state<Record<string, string>>({});
  let formError = $state<string | null>(null);
  let busy = $state(false);
  let secret = $state<string | null>(null);
  let secretName = $state('');
  let del = $state<string | null>(null);
  let delOpen = $state(false);

  const writable = $derived(isGlobalAdmin());
  const expired = (k: AdminApiKey) => !!k.expiresAt && Date.parse(k.expiresAt) <= Date.now();

  function openForm(k: AdminApiKey | null) {
    editName = k?.name ?? null;
    f = k
      ? { name: k.name, description: k.description ?? '', expiresAt: toLocalInput(k.expiresAt), roles: k.roles.map((r) => ({ ...r })) }
      : { name: '', description: '', expiresAt: '', roles: [{ role: 'viewer' }] };
    errors = {};
    formError = null;
    formOpen = true;
  }

  async function submit() {
    const e: Record<string, string> = {};
    if (!editName && !NAME_RE.test(f.name)) e.name = 'Ungültiger Name: a-z, 0-9, „-“, „.“, „_“.';
    if (f.roles.length === 0) e.roles = 'Mindestens eine Rolle.';
    const expiresAt = f.expiresAt ? fromLocalInput(f.expiresAt) : null;
    if (f.expiresAt && (!expiresAt || Date.parse(expiresAt) <= Date.now())) e.expiresAt = 'Zeitpunkt in der Zukunft angeben.';
    errors = e;
    formError = null;
    if (Object.keys(e).length) return;
    busy = true;
    try {
      if (editName) {
        await adminKeys.update(editName, { description: f.description.trim() || null, roles: f.roles, expiresAt });
        toast('success', 'Admin-API-Key gespeichert', editName);
      } else {
        const r = await adminKeys.create({ name: f.name, description: f.description.trim() || null, roles: f.roles, expiresAt });
        secretName = r.key.name;
        secret = r.secret;
      }
      formOpen = false;
      void data.reload();
    } catch (err) {
      handleError(err, { action: 'Admin-API-Key speichern', form: (m) => (formError = m) });
    } finally {
      busy = false;
    }
  }

  async function runDelete(): Promise<string | null> {
    if (!del) return null;
    try {
      await adminKeys.remove(del);
      toast('success', 'Admin-API-Key widerrufen', del);
      void data.reload();
      return null;
    } catch (e) {
      return formErrorOf(e, 'Admin-API-Key löschen');
    }
  }

  const columns: Column<AdminApiKey>[] = [
    { id: 'name', label: 'Name', cell: nameCell, sort: (k) => k.name },
    { id: 'roles', label: 'Rollen', cell: rolesCell },
    { id: 'expires', label: 'Ablauf', cell: expiresCell, sort: (k) => (k.expiresAt ? Date.parse(k.expiresAt) : Infinity), from: 'md' },
    { id: 'used', label: 'Zuletzt benutzt', cell: usedCell, sort: (k) => (k.lastUsedAt ? Date.parse(k.lastUsedAt) : 0), from: 'lg' },
    { id: 'by', label: 'Angelegt von', cell: byCell, sort: (k) => k.createdBy, from: 'xl' },
    { id: 'actions', label: 'Aktionen', cell: actionsCell, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
</script>

{#snippet nameCell(k: AdminApiKey)}
  <span class="block font-medium text-fg">{k.name}</span>
  <span class="block font-mono text-xs text-fg-muted">sk-aigw-admin-{k.prefix}…</span>
  {#if k.description}<span class="block text-xs text-fg-muted">{k.description}</span>{/if}
{/snippet}
{#snippet rolesCell(k: AdminApiKey)}
  <span class="flex flex-wrap gap-1">{#each k.roles as r, i (i)}<Badge tone={r.role === 'admin' && !r.organization && !r.team ? 'accent' : 'neutral'}>{bindingLabel(r)}</Badge>{/each}</span>
{/snippet}
{#snippet expiresCell(k: AdminApiKey)}
  {#if expired(k)}<Badge tone="neutral" dot>abgelaufen</Badge>{:else}<span class="text-sm text-fg-muted">{k.expiresAt ? day(Date.parse(k.expiresAt)) : 'unbefristet'}</span>{/if}
{/snippet}
{#snippet usedCell(k: AdminApiKey)}<span class="text-sm text-fg-muted">{k.lastUsedAt ? ago(Date.parse(k.lastUsedAt)) : 'nie'}</span>{/snippet}
{#snippet byCell(k: AdminApiKey)}<span class="font-mono text-xs text-fg-muted">{k.createdBy}</span>{/snippet}
{#snippet actionsCell(k: AdminApiKey)}
  {#if writable}
    <DropdownMenu.Root>
      <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {k.name}"><Icon name="more" /></DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content class={cx(popover, 'w-44')} align="end" sideOffset={4}>
          <DropdownMenu.Item class={menuItem} onSelect={() => openForm(k)}><Icon name="pencil" size={14} /> Bearbeiten</DropdownMenu.Item>
          <DropdownMenu.Separator class={menuSeparator} />
          <DropdownMenu.Item
            class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')}
            onSelect={() => {
              del = k.name;
              delOpen = true;
            }}><Icon name="trash" size={14} /> Widerrufen …</DropdownMenu.Item
          >
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  {/if}
{/snippet}

<PageHeader title="Admin-API-Keys" description="Zugänge zur Admin-API für Automatisierung (GitOps-Pipeline, Monitoring). Rollen wie bei Nutzern; ein Key mit globaler Admin-Rolle schaltet das Bootstrap-Token ab (Modus „auto“).">
  {#snippet actions()}
    {#if writable}<Button variant="primary" icon="plus" onclick={() => openForm(null)} data-testid="create-admin-key">Admin-API-Key anlegen</Button>{/if}
  {/snippet}
</PageHeader>

  <Card flush>
    <AsyncState loader={data}>
      {#snippet children(rows)}
        <DataTable
          {rows}
          {columns}
          bind:q
          key={(k) => k.name}
          caption="Admin-API-Keys"
          search={(k) => `${k.name} ${k.prefix} ${k.description ?? ''}`}
          sortId="name"
          testid="admin-keys-table"
        >
          {#snippet empty()}
            <EmptyState icon="lock" title="Noch keine Admin-API-Keys" text="Für CI/CD (config:apply) und Monitoring statt persönlicher Anmeldung.">
              {#if writable}<Button variant="primary" icon="plus" onclick={() => openForm(null)}>Admin-API-Key anlegen</Button>{/if}
            </EmptyState>
          {/snippet}
        </DataTable>
      {/snippet}
    </AsyncState>
  </Card>

<FormDialog
  bind:open={formOpen}
  title={editName ? `Admin-API-Key „${editName}“ bearbeiten` : 'Admin-API-Key anlegen'}
  size="lg"
  {busy}
  error={formError}
  submitLabel={editName ? 'Speichern' : 'Anlegen'}
  onsubmit={submit}
  testid="admin-key-form"
>
  {#if !editName}
    <Field label="Name" required hint="a-z, 0-9, „-“, „.“, „_“" error={errors.name}>
      {#snippet children({ id, describedby, invalid })}
        <Input {id} bind:value={f.name} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="gitops-ci" autocomplete="off" />
      {/snippet}
    </Field>
  {/if}
  <Field label="Ablauf" optional hint="Leer = unbefristet (nicht empfohlen)." error={errors.expiresAt}>
    {#snippet children({ id, describedby, invalid })}
      <Input {id} type="datetime-local" bind:value={f.expiresAt} aria-describedby={describedby} aria-invalid={invalid || undefined} />
    {/snippet}
  </Field>
  <Field label="Beschreibung" optional hint="Zweck und Eigentümer." class="sm:col-span-2">
    {#snippet children({ id, describedby })}
      <Input {id} bind:value={f.description} aria-describedby={describedby} autocomplete="off" />
    {/snippet}
  </Field>
  <fieldset class="flex flex-col gap-1.5 sm:col-span-2">
    <legend class="mb-1 text-sm font-medium text-fg">Rollen</legend>
    <RoleEditor bind:value={f.roles} organizations={catalog.orgs.map((o) => o.metadata.name)} teams={catalog.teams.map((t) => t.metadata.name)} />
    {#if errors.roles}<p class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{errors.roles}</p>{/if}
  </fieldset>
</FormDialog>

<SecretDialog bind:secret title="Admin-API-Key „{secretName}“ angelegt" description="Als Bearer-Token an die Admin-API senden (Authorization: Bearer …)." />

<ConfirmDialog bind:open={delOpen} title="Admin-API-Key widerrufen?" confirmLabel="Widerrufen" onconfirm={runDelete}>
  <p>„<span class="font-mono text-fg">{del}</span>“ wird sofort ungültig (401). Automatisierungen mit diesem Key schlagen danach fehl.</p>
</ConfirmDialog>
