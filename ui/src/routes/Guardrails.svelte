<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Guardrails & Plugins (AIGW-36/69/78/77): Plugin-Bindungen aller Ebenen, serverseitig paginiert und gefiltert
  // (Ebene, Plugin); Anlegen/Bearbeiten mit den Steuerfeldern (F117: inherit, locked, failMode, timeoutMs,
  // outputRejectMode) und der Plugin-Konfiguration als JSON (der Server prüft mit der Plugin-Registry, 422).
  // Angezeigt wird der wirksame Fehlermodus (F148) samt Herkunft.
  import { DropdownMenu } from 'bits-ui';
  import { untrack } from 'svelte';
  import { listPage } from '../lib/api/client';
  import { NAME_HINT, NAME_RE, pluginBindingsRes, type PluginBinding, type S } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf, handleError } from '../lib/admin/errors.svelte';
  import FormDialog from '../lib/admin/FormDialog.svelte';
  import { Paged } from '../lib/admin/paged.svelte';
  import { catalog, orgOfTeam } from '../lib/context.svelte';
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
  import Select from '../lib/ui/Select.svelte';
  import { btn, cx, menuItem, menuSeparator, popover, textarea } from '../lib/ui/styles';

  let level = $state('');
  let plugin = $state('');
  const list = new Paged<PluginBinding>((q, l, c) => listPage(pluginBindingsRes.path, q, l, c), () => ({ level: level || undefined, plugin: plugin.trim() || undefined }));
  let first = true;
  $effect(() => {
    void level;
    void plugin;
    if (first) {
      first = false;
      return;
    }
    untrack(() => void list.first());
  });

  const LEVELS: { value: S['ScopeLevel']; label: string }[] = [
    { value: 'global', label: 'global' },
    { value: 'organization', label: 'Organisation' },
    { value: 'team', label: 'Team' },
    { value: 'api-key', label: 'Key' },
    { value: 'model-group', label: 'Modellgruppe' },
  ];
  const PLUGINS = ['pii', 'prompt-filter', 'content-safety', 'webhook', 'header', 'request-transform'];

  // Schreiben je Ebene (F183): global/Modellgruppe globaler Admin, Organisation/Team im jeweiligen Bereich.
  function writableScope(s: S['Scope']): boolean {
    if (s.level === 'organization') return canWrite({ kind: 'in-organization', organization: s.target ?? '' });
    if (s.level === 'team') return canWrite({ kind: 'in-team', organization: orgOfTeam(s.target) ?? '', team: s.target ?? '' });
    if (s.level === 'api-key') return canWrite({ kind: 'global' });
    return canWrite({ kind: 'catalog' });
  }

  // ------------------------------------------------------------ Formular
  const blank = () => ({
    name: '',
    plugin: 'pii',
    level: 'global' as S['ScopeLevel'],
    target: '',
    order: '0',
    enabled: true,
    inherit: '',
    locked: false,
    failMode: '',
    timeoutMs: '',
    outputRejectMode: '',
    config: '{}',
  });
  let f = $state(blank());
  let dlgOpen = $state(false);
  let editName = $state<string | null>(null);
  let etag = $state<string | null>(null);
  let loaded = $state<PluginBinding | null>(null);
  let busy = $state(false);
  let loading = $state(false);
  let formError = $state<string | null>(null);
  let errors = $state<Record<string, string>>({});

  async function openForm(name: string | null) {
    editName = name;
    etag = null;
    loaded = null;
    formError = null;
    errors = {};
    f = blank();
    dlgOpen = true;
    if (!name) return;
    loading = true;
    try {
      const r = await pluginBindingsRes.get(name);
      const s = r.data.spec;
      etag = r.etag;
      loaded = r.data;
      f = {
        name,
        plugin: s.plugin,
        level: s.scope.level,
        target: s.scope.target ?? '',
        order: String(s.order ?? 0),
        enabled: s.enabled !== false,
        inherit: s.inherit ?? '',
        locked: !!s.locked,
        failMode: s.failMode ?? '',
        timeoutMs: s.timeoutMs == null ? '' : String(s.timeoutMs),
        outputRejectMode: s.outputRejectMode ?? '',
        config: JSON.stringify(s.config ?? {}, null, 2),
      };
    } catch (e) {
      handleError(e, { action: 'Bindung laden' });
      dlgOpen = false;
    } finally {
      loading = false;
    }
  }
  async function submit() {
    const e: Record<string, string> = {};
    if (!editName && !NAME_RE.test(f.name)) e.name = 'Ungültiger Name.';
    if (!f.plugin.trim()) e.plugin = 'Plugin angeben.';
    if (f.level !== 'global' && !f.target.trim()) e.target = 'Ziel angeben.';
    const order = Number(f.order);
    if (!Number.isInteger(order)) e.order = 'Ganze Zahl.';
    const timeout = f.timeoutMs.trim() ? Number(f.timeoutMs) : null;
    if (timeout !== null && (!Number.isInteger(timeout) || timeout < 1)) e.timeoutMs = 'Ganze Zahl ≥ 1 (Millisekunden).';
    let config: Record<string, unknown> = {};
    try {
      const v = JSON.parse(f.config || '{}');
      if (!v || typeof v !== 'object' || Array.isArray(v)) throw new Error();
      config = v;
    } catch {
      e.config = 'Kein gültiges JSON-Objekt.';
    }
    errors = e;
    formError = null;
    if (Object.keys(e).length) return;
    const name = editName ?? f.name;
    const spec = {
      ...(loaded?.spec ?? {}),
      plugin: f.plugin.trim(),
      scope: { level: f.level, target: f.level === 'global' ? null : f.target.trim() },
      order,
      enabled: f.enabled,
      inherit: (f.inherit || null) as S['PluginInherit'] | null,
      locked: f.locked,
      failMode: (f.failMode || null) as S['PluginFailMode'] | null,
      timeoutMs: timeout,
      outputRejectMode: (f.outputRejectMode || null) as S['OutputRejectMode'] | null,
      config,
    } as S['PluginBindingSpec'];
    busy = true;
    try {
      if (editName) await pluginBindingsRes.update(name, { metadata: { name, labels: loaded?.metadata.labels ?? {} }, spec }, etag);
      else await pluginBindingsRes.create({ metadata: { name }, spec });
      dlgOpen = false;
      toast('success', 'Plugin-Bindung gespeichert', name);
      void list.reload();
    } catch (err) {
      handleError(err, { action: 'Bindung speichern', form: (m) => (formError = m), reload: () => editName && openForm(editName) });
    } finally {
      busy = false;
    }
  }

  let del = $state<string | null>(null);
  let delOpen = $state(false);
  async function runDelete(): Promise<string | null> {
    if (!del) return null;
    try {
      const { etag: tag } = await pluginBindingsRes.get(del);
      await pluginBindingsRes.remove(del, tag);
      toast('success', 'Plugin-Bindung gelöscht', del);
      void list.reload();
      return null;
    } catch (e) {
      return formErrorOf(e, 'Bindung löschen');
    }
  }

  const targets = $derived(f.level === 'organization' ? catalog.orgs.map((o) => o.metadata.name) : f.level === 'team' ? catalog.teams.map((t) => t.metadata.name) : []);
  const columns: Column<PluginBinding>[] = [
    { id: 'name', label: 'Bindung', cell: nameCell },
    { id: 'scope', label: 'Ebene', cell: scopeCell },
    { id: 'fail', label: 'Fehlermodus', cell: failCell },
    { id: 'control', label: 'Steuerung', cell: controlCell, from: 'lg' },
    { id: 'actions', label: 'Aktionen', cell: actionsCell, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
</script>

{#snippet nameCell(b: PluginBinding)}
  <span class="flex items-center gap-2"><Badge mono>{b.spec.plugin}</Badge><span class="font-mono text-sm">{b.metadata.name}</span>{#if b.spec.enabled === false}<Badge tone="neutral">aus</Badge>{/if}</span>
{/snippet}
{#snippet scopeCell(b: PluginBinding)}<span class="text-sm">{LEVELS.find((l) => l.value === b.spec.scope.level)?.label}{b.spec.scope.target ? ` · ${b.spec.scope.target}` : ''}</span>{/snippet}
{#snippet failCell(b: PluginBinding)}
  {#if b.effective}
    <Badge tone={b.effective.failMode === 'closed' ? 'warning' : 'info'} dot>{b.effective.failMode}</Badge>
    <span class="ml-1 text-xs text-fg-subtle">{b.effective.failModeSource === 'binding' ? 'Bindung' : 'Standard'} · {b.effective.timeoutMs} ms</span>
  {/if}
{/snippet}
{#snippet controlCell(b: PluginBinding)}
  <span class="flex flex-wrap gap-1 text-xs">
    <Badge tone="outline">Reihenfolge {b.spec.order ?? 0}</Badge>
    {#if b.spec.inherit}<Badge tone="outline">erben: {b.spec.inherit}</Badge>{/if}
    {#if b.spec.locked}<Badge tone="accent" icon="lock">gesperrt</Badge>{/if}
    {#if b.effective?.outputRejectMode}<Badge tone="outline">Ablehnung: {b.effective.outputRejectMode}</Badge>{/if}
  </span>
{/snippet}
{#snippet actionsCell(b: PluginBinding)}
  {#if writableScope(b.spec.scope)}
    <DropdownMenu.Root>
      <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {b.metadata.name}"><Icon name="more" /></DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content class={cx(popover, 'w-44')} align="end" sideOffset={4}>
          <DropdownMenu.Item class={menuItem} onSelect={() => openForm(b.metadata.name)}><Icon name="pencil" size={14} /> Bearbeiten</DropdownMenu.Item>
          <DropdownMenu.Separator class={menuSeparator} />
          <DropdownMenu.Item
            class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')}
            onSelect={() => {
              del = b.metadata.name;
              delOpen = true;
            }}><Icon name="trash" size={14} /> Löschen …</DropdownMenu.Item
          >
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  {/if}
{/snippet}

<PageHeader title="Guardrails & Plugins" description="Plugin-Bindungen je Ebene (global, Organisation, Team, Key, Modellgruppe). Die Kette läuft von außen nach innen, Antworten in umgekehrter Reihenfolge.">
  {#snippet actions()}
    {#if canWrite({ kind: 'catalog' })}<Button variant="primary" icon="plus" onclick={() => openForm(null)} data-testid="create-binding">Bindung anlegen</Button>{/if}
  {/snippet}
</PageHeader>

<Card flush>
  <AsyncState loader={list}>
    {#snippet children(rows)}
      <DataTable
        {rows}
        {columns}
        server={list}
        filtered={!!level || !!plugin}
        key={(b) => b.metadata.name}
        caption="Plugin-Bindungen"
        search={() => ''}
        searchPlaceholder="Name …"
        testid="bindings-table"
        resetFilters={() => {
          level = '';
          plugin = '';
        }}
      >
        {#snippet toolbar()}
          <div class="w-40"><Select size="sm" label="Ebene" bind:value={level} items={[{ value: '', label: 'Alle Ebenen' }, ...LEVELS]} /></div>
          <div class="w-40"><Select size="sm" label="Plugin" bind:value={plugin} items={[{ value: '', label: 'Alle Plugins' }, ...PLUGINS.map((p) => ({ value: p, label: p }))]} /></div>
        {/snippet}
        {#snippet empty()}<EmptyState icon="shield" title="Noch keine Plugin-Bindungen" text="Bindungen schalten Guardrails (PII, Prompt-Filter, content-safety) und Webhooks für eine Ebene ein." />{/snippet}
      </DataTable>
    {/snippet}
  </AsyncState>
</Card>

<FormDialog bind:open={dlgOpen} title={editName ? `Bindung „${editName}“ bearbeiten` : 'Plugin-Bindung anlegen'} size="lg" {busy} {loading} error={formError} submitLabel={editName ? 'Speichern' : 'Anlegen'} onsubmit={submit} testid="binding-form">
  {#if !editName}
    <Field label="Name" required hint={NAME_HINT} error={errors.name}>
      {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={f.name} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" placeholder="pii-global" />{/snippet}
    </Field>
  {/if}
  <Field label="Plugin" required error={errors.plugin}>
    {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={f.plugin} aria-describedby={describedby} aria-invalid={invalid || undefined} list="plugin-names" autocomplete="off" />{/snippet}
  </Field>
  <datalist id="plugin-names">{#each PLUGINS as p (p)}<option value={p}></option>{/each}</datalist>
  <Field label="Ebene">
    {#snippet children({ id })}<Select {id} bind:value={f.level} items={LEVELS} />{/snippet}
  </Field>
  {#if f.level !== 'global'}
    <Field label="Ziel" required error={errors.target} hint="Name der Organisation, des Teams, Keys bzw. der Modellgruppe.">
      {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={f.target} aria-describedby={describedby} aria-invalid={invalid || undefined} list="binding-targets" autocomplete="off" />{/snippet}
    </Field>
    <datalist id="binding-targets">{#each targets as t (t)}<option value={t}></option>{/each}</datalist>
  {/if}
  <Field label="Reihenfolge" error={errors.order} hint="aufsteigend innerhalb der Ebene">
    {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={f.order} inputmode="numeric" aria-describedby={describedby} aria-invalid={invalid || undefined} />{/snippet}
  </Field>
  <Field label="Aktiv">
    {#snippet children({ id })}<Select {id} value={f.enabled ? 'true' : 'false'} onchange={(v) => (f.enabled = v === 'true')} items={[{ value: 'true', label: 'an' }, { value: 'false', label: 'aus' }]} />{/snippet}
  </Field>
  <Field label="Fehlermodus" hint="bei Laufzeitfehler/Timeout: closed = ablehnen, open = überspringen (F148)">
    {#snippet children({ id })}<Select {id} bind:value={f.failMode} items={[{ value: '', label: 'Standard des Plugins' }, { value: 'closed', label: 'closed – ablehnen' }, { value: 'open', label: 'open – überspringen' }]} />{/snippet}
  </Field>
  <Field label="Zeitlimit (ms)" optional error={errors.timeoutMs}>
    {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={f.timeoutMs} inputmode="numeric" placeholder="Standard" aria-describedby={describedby} aria-invalid={invalid || undefined} />{/snippet}
  </Field>
  <Field label="Ausgabe-Ablehnung" hint="finish: 200 mit content_filter/refusal · error: 400 (F160)">
    {#snippet children({ id })}<Select {id} bind:value={f.outputRejectMode} items={[{ value: '', label: 'Standard (finish)' }, { value: 'finish', label: 'finish' }, { value: 'error', label: 'error' }]} />{/snippet}
  </Field>
  <Field label="Vererbung" hint="wie Bindungen darunterliegender Ebenen wirken (F117)">
    {#snippet children({ id })}<Select {id} bind:value={f.inherit} items={[{ value: '', label: 'Standard' }, { value: 'append', label: 'append – ergänzen' }, { value: 'override', label: 'override – ersetzen' }, { value: 'disable', label: 'disable – abschalten' }]} />{/snippet}
  </Field>
  <Field label="Gesperrt" hint="untere Ebenen dürfen die Bindung nicht ändern/abschalten">
    {#snippet children({ id })}<Select {id} value={f.locked ? 'true' : 'false'} onchange={(v) => (f.locked = v === 'true')} items={[{ value: 'false', label: 'nein' }, { value: 'true', label: 'ja' }]} />{/snippet}
  </Field>
  <Field label="Konfiguration (JSON)" error={errors.config} hint="Schema je Plugin; der Server prüft mit der Plugin-Registry (422)." class="sm:col-span-2">
    {#snippet children({ id, describedby, invalid })}<textarea {id} class={[textarea, 'min-h-32']} bind:value={f.config} aria-describedby={describedby} aria-invalid={invalid || undefined} spellcheck="false"></textarea>{/snippet}
  </Field>
</FormDialog>

<ConfirmDialog bind:open={delOpen} title="Plugin-Bindung löschen?" confirmLabel="Löschen" onconfirm={runDelete}>
  <p>„<span class="font-mono text-fg">{del}</span>“ wird gelöscht; das Plugin greift nach dem Hot Reload auf dieser Ebene nicht mehr.</p>
</ConfirmDialog>
