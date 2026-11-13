<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Secrets (F149, AIGW-78/77): allgemeine Geheimnisse für Webhook-, content-safety- und MCP-Verweise. Werte sind nur
  // schreibbar – die Liste zeigt Feldnamen. Anlegen, „Werte ändern“ (PATCH: einzelne Felder setzen/entfernen),
  // Löschen (409 mit Hinweis auf verwendende Objekte). Schreiben nur globaler Admin (Katalog).
  import { DropdownMenu } from 'bits-ui';
  import { listPage } from '../lib/api/client';
  import { FIELD_NAME, NAME_HINT, NAME_RE, secrets, type S, type Secret } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf, handleError } from '../lib/admin/errors.svelte';
  import FormDialog from '../lib/admin/FormDialog.svelte';
  import { Paged } from '../lib/admin/paged.svelte';
  import { ago } from '../lib/format';
  import { canWrite } from '../lib/rbac.svelte';
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
  import { btn, cx, input, menuItem, menuSeparator, popover } from '../lib/ui/styles';

  const list = new Paged<Secret>((q, l, c) => listPage(secrets.path, q, l, c));
  const writable = $derived(canWrite({ kind: 'catalog' }));

  // ------------------------------------------------------------ Anlegen / Werte ändern
  type Row = { field: string; value: string; remove: boolean; existing: boolean };
  let dlgOpen = $state(false);
  let editName = $state<string | null>(null);
  let etag = $state<string | null>(null);
  let f = $state({ name: '', description: '', rows: [] as Row[] });
  let busy = $state(false);
  let formError = $state<string | null>(null);
  let errors = $state<Record<string, string>>({});

  async function openForm(name: string | null) {
    editName = name;
    formError = null;
    errors = {};
    etag = null;
    f = { name: '', description: '', rows: [{ field: 'value', value: '', remove: false, existing: false }] };
    dlgOpen = true;
    if (name) {
      try {
        const r = await secrets.get(name);
        etag = r.etag;
        f = { name, description: r.data.spec.description ?? '', rows: r.data.spec.fields.map((x) => ({ field: x, value: '', remove: false, existing: true })) };
      } catch (e) {
        handleError(e, { action: 'Secret laden' });
        dlgOpen = false;
      }
    }
  }
  async function submit() {
    const e: Record<string, string> = {};
    if (!editName && !NAME_RE.test(f.name)) e.name = 'Ungültiger Name: a-z, 0-9, „-“, „.“, „_“, beginnt alphanumerisch.';
    if (f.rows.some((r) => !FIELD_NAME.test(r.field))) e.rows = 'Feldnamen: Buchstabe, dann Buchstaben/Ziffern/_ (max. 64).';
    if (!editName && f.rows.some((r) => !r.value)) e.rows = 'Jedes Feld braucht einen Wert.';
    if (editName && f.rows.some((r) => !r.existing && !r.value)) e.rows = 'Neue Felder brauchen einen Wert.';
    errors = e;
    formError = null;
    if (Object.keys(e).length) return;
    busy = true;
    try {
      if (!editName) {
        const data = Object.fromEntries(f.rows.map((r) => [r.field, { value: r.value }]));
        await secrets.create({ metadata: { name: f.name }, spec: { description: f.description.trim() || null, data } });
      } else {
        const data: Record<string, S['SecretValue'] | null> = {};
        for (const r of f.rows) {
          if (r.remove) data[r.field] = null;
          else if (r.value) data[r.field] = { value: r.value };
        }
        await secrets.patch(editName, { spec: { description: f.description.trim() || null, data: data as Record<string, S['SecretValue']> } }, etag);
      }
      dlgOpen = false;
      toast('success', 'Secret gespeichert', editName ?? f.name);
      void list.reload();
    } catch (err) {
      handleError(err, { action: 'Secret speichern', form: (m) => (formError = m), reload: () => editName && openForm(editName) });
    } finally {
      busy = false;
    }
  }

  // ------------------------------------------------------------ Löschen
  let del = $state<string | null>(null);
  let delOpen = $state(false);
  async function runDelete(): Promise<string | null> {
    if (!del) return null;
    try {
      const { etag: tag } = await secrets.get(del);
      await secrets.remove(del, tag);
      toast('success', 'Secret gelöscht', del);
      void list.reload();
      return null;
    } catch (e) {
      return formErrorOf(e, 'Secret löschen', () => void list.reload());
    }
  }

  const columns: Column<Secret>[] = [
    { id: 'name', label: 'Name', cell: nameCell },
    { id: 'fields', label: 'Felder', cell: fieldsCell },
    { id: 'updated', label: 'Geändert', cell: updatedCell, from: 'md' },
    { id: 'actions', label: 'Aktionen', cell: actionsCell, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
</script>

{#snippet nameCell(s: Secret)}
  <span class="block font-mono text-sm font-medium">{s.metadata.name}</span>
  {#if s.spec.description}<span class="block text-xs text-fg-muted">{s.spec.description}</span>{/if}
{/snippet}
{#snippet fieldsCell(s: Secret)}<span class="flex flex-wrap gap-1">{#each s.spec.fields as x (x)}<Badge mono tone="outline">{x}</Badge>{/each}</span>{/snippet}
{#snippet updatedCell(s: Secret)}<span class="text-sm text-fg-muted">{s.metadata.updatedAt ? ago(Date.parse(s.metadata.updatedAt)) : '–'}</span>{/snippet}
{#snippet actionsCell(s: Secret)}
  {#if writable}
    <DropdownMenu.Root>
      <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {s.metadata.name}"><Icon name="more" /></DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content class={cx(popover, 'w-44')} align="end" sideOffset={4}>
          <DropdownMenu.Item class={menuItem} onSelect={() => openForm(s.metadata.name)}><Icon name="pencil" size={14} /> Werte ändern</DropdownMenu.Item>
          <DropdownMenu.Separator class={menuSeparator} />
          <DropdownMenu.Item
            class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')}
            onSelect={() => {
              del = s.metadata.name;
              delOpen = true;
            }}><Icon name="trash" size={14} /> Löschen …</DropdownMenu.Item
          >
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  {/if}
{/snippet}

<PageHeader title="Secrets" description="Geheimnisse für Webhooks, content-safety und MCP-Server – verschlüsselt gespeichert, nie lesbar. Verweise per Name.">
  {#snippet actions()}
    {#if writable}<Button variant="primary" icon="plus" onclick={() => openForm(null)} data-testid="create-secret">Secret anlegen</Button>{/if}
  {/snippet}
</PageHeader>

<Card flush>
  <AsyncState loader={list}>
    {#snippet children(rows)}
      <DataTable {rows} {columns} server={list} key={(s) => s.metadata.name} caption="Secrets" search={() => ''} searchPlaceholder="Name …" testid="secrets-table">
        {#snippet empty()}<EmptyState icon="lock" title="Noch keine Secrets" text="Secrets halten z. B. HMAC-Schlüssel für Webhooks oder Tokens für MCP-Server." />{/snippet}
      </DataTable>
    {/snippet}
  </AsyncState>
</Card>

<FormDialog bind:open={dlgOpen} title={editName ? `Werte von „${editName}“ ändern` : 'Secret anlegen'} {busy} error={formError} submitLabel={editName ? 'Speichern' : 'Anlegen'} onsubmit={submit} testid="secret-form">
  {#if !editName}
    <Field label="Name" required hint={NAME_HINT} error={errors.name}>
      {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={f.name} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" placeholder="webhook-hmac" />{/snippet}
    </Field>
  {/if}
  <Field label="Beschreibung" optional class={editName ? 'sm:col-span-2' : ''}>
    {#snippet children({ id })}<Input {id} bind:value={f.description} autocomplete="off" />{/snippet}
  </Field>
  <fieldset class="flex flex-col gap-2 sm:col-span-2">
    <legend class="mb-1 text-sm font-medium text-fg">Werte <span class="text-xs font-normal text-fg-subtle">{editName ? '(leer = unverändert; Werte sind nie lesbar)' : ''}</span></legend>
    {#each f.rows as r, i (i)}
      <div class="flex items-center gap-2">
        <input class={[input, 'w-40 font-mono text-sm']} aria-label="Feldname {i + 1}" bind:value={r.field} disabled={r.existing} />
        <input class={[input, 'flex-1']} type="password" autocomplete="new-password" aria-label="Wert {i + 1}" bind:value={r.value} disabled={r.remove} placeholder={r.existing ? '•••••• (unverändert)' : 'Wert'} />
        {#if r.existing}
          <label class="flex items-center gap-1 text-xs text-fg-muted"><input type="checkbox" bind:checked={r.remove} /> entfernen</label>
        {:else}
          <button type="button" class={btn('ghost', 'icon-sm')} aria-label="Feld {i + 1} entfernen" onclick={() => (f.rows = f.rows.filter((_, j) => j !== i))}><Icon name="x" size={14} /></button>
        {/if}
      </div>
    {/each}
    {#if errors.rows}<p class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{errors.rows}</p>{/if}
    <div><Button size="sm" icon="plus" onclick={() => (f.rows = [...f.rows, { field: '', value: '', remove: false, existing: false }])}>Feld hinzufügen</Button></div>
  </fieldset>
</FormDialog>

<ConfirmDialog bind:open={delOpen} title="Secret löschen?" confirmLabel="Löschen" onconfirm={runDelete}>
  <p>„<span class="font-mono text-fg">{del}</span>“ wird gelöscht. Nicht möglich, solange Webhooks, Plugins oder MCP-Server darauf verweisen (der Server nennt sie).</p>
</ConfirmDialog>
