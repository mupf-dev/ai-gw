<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Guardrails & Plugins (AIGW-36/69/78/77): Plugin-Bindungen aller Ebenen, serverseitig paginiert und gefiltert
  // (Ebene, Plugin); Anlegen/Bearbeiten mit den Steuerfeldern (F117: inherit, locked, failMode, timeoutMs,
  // outputRejectMode) und der Plugin-Konfiguration als JSON (der Server prüft mit der Plugin-Registry, 422).
  // Angezeigt wird der wirksame Fehlermodus (F148) samt Herkunft.
  // AIGW-52: strukturierte Formulare je Plugin (pii, prompt-filter, content-safety, webhook; F214) mit JSON-Ansicht,
  // Testlauf mit Vorschau (POST /plugin-bindings:test), Kettenvorschau je Kontext (Zwiebelprinzip),
  // Standard-Fehlermodus je Plugin-Art aus GET /plugins, Backend-Fehler am betroffenen Feld.
  // AIGW-86: Reiter „Profile“ (Standard-Guardrail-Profile anwenden), Hinweis ohne aktive Guardrails.
  import { DropdownMenu, Tabs } from 'bits-ui';
  import { untrack } from 'svelte';
  import { ApiError, listPage } from '../lib/api/client';
  import { credentials, GUARDRAIL_PLUGINS, modelGroups, NAME_HINT, NAME_RE, pluginBindingsRes, pluginCatalog, secrets, type PluginBinding, type S } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf, handleError, Loader } from '../lib/admin/errors.svelte';
  import ChainView from '../lib/admin/plugins/ChainView.svelte';
  import GuardrailProfiles from '../lib/admin/plugins/GuardrailProfiles.svelte';
  import PluginConfigForm, { STRUCTURED } from '../lib/admin/plugins/PluginConfigForm.svelte';
  import PluginTest from '../lib/admin/plugins/PluginTest.svelte';
  import FormDialog from '../lib/admin/FormDialog.svelte';
  import { Paged } from '../lib/admin/paged.svelte';
  import { catalog, orgOfTeam } from '../lib/context.svelte';
  import { canWrite } from '../lib/rbac.svelte';
  import { route } from '../lib/router.svelte';
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
  import { btn, cx, menuItem, menuSeparator, popover, tabsList, tabsTrigger, textarea } from '../lib/ui/styles';

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
  // Plugin-Katalog (Standard-Fehlermodus je Plugin-Art, F148); ohne Antwort die bekannten eingebauten Plugins.
  const plugins = new Loader(() => pluginCatalog());
  const FALLBACK = ['cache', 'content-safety', 'header', 'pii', 'prompt-filter', 'request-transform', 'webhook'];
  const PLUGINS = $derived(plugins.data?.map((p) => p.name) ?? FALLBACK);
  const infoOf = (name: string) => plugins.data?.find((p) => p.name === name);
  const testable = (name: string) => infoOf(name)?.testable ?? (name === 'pii' || name === 'prompt-filter');

  // Alle sichtbaren Bindungen für die Kettenvorschau.
  let tab = $state(new URLSearchParams(route.search).get('tab') === 'profiles' ? 'profiles' : 'bindings');
  // Hinweis, wenn keine Guardrail-Bindung (pii, prompt-filter, content-safety) aktiv ist.
  const guarded = new Loader(async () => (await pluginBindingsRes.list()).some((b) => b.spec.enabled !== false && GUARDRAIL_PLUGINS.includes(b.spec.plugin)));
  const all = new Loader(() => (tab === 'chain' ? pluginBindingsRes.list() : Promise.resolve([] as PluginBinding[])));
  $effect(() => {
    if (tab === 'chain') untrack(() => void all.reload());
  });
  // Verweise für die Formulare (nur Namen – Werte sind nie lesbar).
  let refs = $state<{ secrets: string[]; credentials: string[]; groups: string[] }>({ secrets: [], credentials: [], groups: [] });
  let refsLoaded = false;
  async function loadRefs() {
    if (refsLoaded) return;
    refsLoaded = true;
    const names = async (p: Promise<{ metadata: { name: string } }[]>) => (await p.catch(() => [])).map((x) => x.metadata.name);
    const [s, c, g] = await Promise.all([names(secrets.list()), names(credentials.list()), names(modelGroups.list())]);
    refs = { secrets: s, credentials: c, groups: g };
  }

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
  });
  /** Plugin-Konfiguration (strukturiertes Formular arbeitet direkt darauf). */
  let cfg = $state<Record<string, unknown>>({});
  /** JSON-Ansicht (Pflicht für Plugins ohne Formular). */
  let jsonMode = $state(false);
  let cfgText = $state('{}');
  function toJson() {
    cfgText = JSON.stringify(cfg, null, 2);
    jsonMode = true;
  }
  function fromJson(): boolean {
    const v = parseCfg();
    if (!v) {
      errors = { ...errors, config: 'Kein gültiges JSON-Objekt.' };
      return false;
    }
    cfg = v;
    jsonMode = false;
    return true;
  }
  function parseCfg(): Record<string, unknown> | null {
    if (!showJson) return $state.snapshot(cfg);
    try {
      const v = JSON.parse(cfgText || '{}');
      return v && typeof v === 'object' && !Array.isArray(v) ? v : null;
    } catch {
      return null;
    }
  }
  let f = $state(blank());
  const structured = $derived(STRUCTURED.has(f.plugin.trim()));
  const showJson = $derived(jsonMode || !structured);
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
    cfg = {};
    cfgText = '{}';
    jsonMode = false;
    dlgOpen = true;
    void loadRefs();
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
      };
      cfg = structuredClone(s.config ?? {}) as Record<string, unknown>;
      cfgText = JSON.stringify(cfg, null, 2);
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
    const config = parseCfg() ?? {};
    if (!parseCfg()) e.config = 'Kein gültiges JSON-Objekt.';
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
      void guarded.reload();
      if (tab === 'chain') void all.reload();
    } catch (err) {
      if (registryError(err)) return;
      handleError(err, { action: 'Bindung speichern', form: (m) => (formError = m), reload: () => editName && openForm(editName), field: fieldError });
    } finally {
      busy = false;
    }
  }

  /** Backend-Fehler am Feld (Präfix „feld: …“ bzw. „Plugin-Bindung (x): …“ der Registry-Prüfung). */
  const FIELDS: Record<string, string> = {
    'metadata.name': 'name',
    name: 'name',
    plugin: 'plugin',
    'scope.target': 'target',
    'scope.level': 'target',
    order: 'order',
    timeoutMs: 'timeoutMs',
    failMode: 'failMode',
    inherit: 'inherit',
    outputRejectMode: 'outputRejectMode',
  };
  function fieldError(field: string, message: string): boolean {
    const k = FIELDS[field] ?? (field.startsWith('config') ? 'config' : null);
    if (!k) return false;
    errors = { ...errors, [k]: message };
    return true;
  }
  /** Meldungen der Registry-Prüfung (`Plugin-Bindung (pii): …`): Steuerfeld oder Konfiguration. */
  function registryError(err: unknown): boolean {
    if (!(err instanceof ApiError) || err.status !== 422 || !err.detail?.startsWith('Plugin-Bindung (')) return false;
    const msg = err.detail.replace(/^Plugin-Bindung \([^)]*\): /, '');
    const ctl = ['timeoutMs', 'failMode', 'outputRejectMode', 'inherit', 'locked'].find((k) => msg.includes(k));
    errors = { ...errors, [ctl ?? 'config']: msg };
    formError = 'Bitte die markierten Felder prüfen.';
    return true;
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
      void guarded.reload();
      if (tab === 'chain') void all.reload();
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

{#if guarded.data === false}
  <p class="mb-4 flex flex-wrap items-center gap-2 rounded-md border border-warning-fg/30 bg-warning-bg px-3 py-2 text-sm text-warning-fg" role="status" data-testid="no-guardrails">
    Keine Guardrails aktiv – Anfragen und Antworten werden nicht auf PII, Geheimnisse oder Prompt-Injection geprüft.
    {#if tab !== 'profiles'}<Button size="sm" variant="ghost" onclick={() => (tab = 'profiles')}>Profil auswählen</Button>{/if}
  </p>
{/if}

<Tabs.Root bind:value={tab}>
  <Tabs.List class={cx(tabsList, 'mb-4')}>
    <Tabs.Trigger value="bindings" class={tabsTrigger} data-testid="tab-bindings">Bindungen</Tabs.Trigger>
    <Tabs.Trigger value="chain" class={tabsTrigger} data-testid="tab-chain">Kette</Tabs.Trigger>
    <Tabs.Trigger value="profiles" class={tabsTrigger} data-testid="tab-profiles">Profile</Tabs.Trigger>
  </Tabs.List>
  <Tabs.Content value="profiles">
    <GuardrailProfiles
      onapplied={() => {
        void list.reload();
        void guarded.reload();
        if (tab === 'chain') void all.reload();
      }}
    />
  </Tabs.Content>
  <Tabs.Content value="bindings">
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
  </Tabs.Content>
  <Tabs.Content value="chain">
    <Card>
      <AsyncState loader={all}>
        {#snippet children(rows)}<ChainView bindings={rows} onedit={(n) => openForm(n)} />{/snippet}
      </AsyncState>
    </Card>
  </Tabs.Content>
</Tabs.Root>

<FormDialog bind:open={dlgOpen} title={editName ? `Bindung „${editName}“ bearbeiten` : 'Plugin-Bindung anlegen'} size="lg" {busy} {loading} error={formError} submitLabel={editName ? 'Speichern' : 'Anlegen'} onsubmit={submit} testid="binding-form">
  {#if !editName}
    <Field label="Name" required hint={NAME_HINT} error={errors.name}>
      {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={f.name} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" placeholder="pii-global" />{/snippet}
    </Field>
  {/if}
  <Field label="Plugin" required error={errors.plugin}>
    {#snippet children({ id, describedby, invalid })}<Select
        {id}
        bind:value={f.plugin}
        onchange={() => {
          // Andere Plugin-Art: Konfiguration passt nicht mehr.
          cfg = {};
          cfgText = '{}';
        }}
        items={PLUGINS.map((p) => ({ value: p, label: p }))}
        {describedby}
        {invalid}
      />{/snippet}
  </Field>
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
  <Field label="Fehlermodus" error={errors.failMode} hint="bei Laufzeitfehler/Timeout: closed = ablehnen, open = überspringen (F148)">
    {#snippet children({ id })}<Select {id} bind:value={f.failMode} items={[{ value: '', label: `Standard des Plugins${infoOf(f.plugin) ? ` (${infoOf(f.plugin)?.defaultFailMode})` : ''}` }, { value: 'closed', label: 'closed – ablehnen' }, { value: 'open', label: 'open – überspringen' }]} />{/snippet}
  </Field>
  <Field label="Zeitlimit (ms)" optional error={errors.timeoutMs} hint={infoOf(f.plugin) ? `Standard ${infoOf(f.plugin)?.defaultTimeoutMs} ms` : undefined}>
    {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={f.timeoutMs} inputmode="numeric" placeholder="Standard" aria-describedby={describedby} aria-invalid={invalid || undefined} />{/snippet}
  </Field>
  <Field label="Ausgabe-Ablehnung" error={errors.outputRejectMode} hint="finish: 200 mit content_filter/refusal · error: 400 (F160)">
    {#snippet children({ id })}<Select {id} bind:value={f.outputRejectMode} items={[{ value: '', label: 'Standard (finish)' }, { value: 'finish', label: 'finish' }, { value: 'error', label: 'error' }]} />{/snippet}
  </Field>
  <Field label="Vererbung" error={errors.inherit} hint="wie Bindungen darunterliegender Ebenen wirken (F117)">
    {#snippet children({ id })}<Select {id} bind:value={f.inherit} items={[{ value: '', label: 'Standard' }, { value: 'append', label: 'append – ergänzen' }, { value: 'override', label: 'override – ersetzen' }, { value: 'disable', label: 'disable – abschalten' }]} />{/snippet}
  </Field>
  <Field label="Gesperrt" hint="untere Ebenen dürfen die Bindung nicht ändern/abschalten">
    {#snippet children({ id })}<Select {id} value={f.locked ? 'true' : 'false'} onchange={(v) => (f.locked = v === 'true')} items={[{ value: 'false', label: 'nein' }, { value: 'true', label: 'ja' }]} />{/snippet}
  </Field>
  <div class="flex items-center gap-2 border-t border-line pt-4 sm:col-span-2">
    <h3 class="text-sm font-medium">Konfiguration</h3>
    {#if structured}
      <Button size="sm" variant="ghost" class="ml-auto" icon={showJson ? 'sliders' : 'file-text'} onclick={() => (showJson ? fromJson() : toJson())} data-testid="config-mode">{showJson ? 'Formular' : 'JSON bearbeiten'}</Button>
    {/if}
  </div>
  {#if showJson}
    <Field label="Konfiguration (JSON)" error={errors.config} hint="Schema je Plugin; der Server prüft mit der Plugin-Registry (422)." class="sm:col-span-2">
      {#snippet children({ id, describedby, invalid })}<textarea {id} class={[textarea, 'min-h-32 font-mono']} bind:value={cfgText} aria-describedby={describedby} aria-invalid={invalid || undefined} spellcheck="false" data-testid="config-json"></textarea>{/snippet}
    </Field>
  {:else}
    <PluginConfigForm plugin={f.plugin} bind:config={cfg} secrets={refs.secrets} credentials={refs.credentials} modelGroups={refs.groups} secretRefs={canWrite({ kind: 'catalog' })} />
    {#if errors.config}<p class="text-sm text-danger-fg sm:col-span-2" role="alert" data-testid="config-error">{errors.config}</p>{/if}
  {/if}
  {#if testable(f.plugin)}
    <PluginTest plugin={f.plugin} config={parseCfg} outputRejectMode={f.outputRejectMode} />
  {/if}
</FormDialog>

<ConfirmDialog bind:open={delOpen} title="Plugin-Bindung löschen?" confirmLabel="Löschen" onconfirm={runDelete}>
  <p>„<span class="font-mono text-fg">{del}</span>“ wird gelöscht; das Plugin greift nach dem Hot Reload auf dieser Ebene nicht mehr.</p>
</ConfirmDialog>
