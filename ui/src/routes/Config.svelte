<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // YAML-Import/-Export (AIGW-55) über die GitOps-Logik des Servers (config:export / config:apply):
  // - Export gesamt oder Auswahl (Arten, Organisation, Team, einzelne Ressource) mit Vorschau und Download;
  //   Geheimwerte nie im Klartext (Zugangsdaten/Secrets als valueFrom-Verweise, API-Keys ohne Klartext).
  // - Import: Editor mit Zeilennummern, Trockenlauf mit Diff (PlanView), Validierungsfehler mit Zeilenbezug
  //   (Problem.issues), Anwenden erst nach Bestätigung (bei Löschungen mit Eingabe „ANWENDEN“).
  // Rechte (F55): Export globale Leserollen und `gitops`, Apply (auch Trockenlauf) globaler `admin` und `gitops`;
  // im Nur-YAML-Modus bleibt Apply erlaubt. Vorbelegung per URL (?kinds=&name=&organization=&team=, Detailseiten).
  import { Checkbox } from 'bits-ui';
  import { ApiError, applyConfig, exportConfig, type ExportSelection, type Schemas } from '../lib/api/client';
  import { download, handleError, problemText } from '../lib/admin/errors.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import FormError from '../lib/admin/FormError.svelte';
  import { catalog } from '../lib/context.svelte';
  import { canReadGlobal, isGlobalAdmin, roles } from '../lib/rbac.svelte';
  import { route } from '../lib/router.svelte';
  import { auth } from '../lib/session.svelte';
  import { toast } from '../lib/toast.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import CodeBlock from '../lib/ui/CodeBlock.svelte';
  import Field from '../lib/ui/Field.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import Input from '../lib/ui/Input.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Select from '../lib/ui/Select.svelte';
  import { checkbox } from '../lib/ui/styles';
  import YamlEditor from './config/YamlEditor.svelte';
  import PlanView from './config/PlanView.svelte';

  type Kind = Schemas['ResourceKind'];
  const KINDS: { kind: Kind; label: string }[] = [
    { kind: 'Organization', label: 'Organisationen' },
    { kind: 'Team', label: 'Teams' },
    { kind: 'User', label: 'Nutzer' },
    { kind: 'ApiKey', label: 'API-Keys' },
    { kind: 'ProviderCredential', label: 'Zugangsdaten' },
    { kind: 'Secret', label: 'Secrets' },
    { kind: 'ModelGroup', label: 'Modellgruppen' },
    { kind: 'Deployment', label: 'Deployments' },
    { kind: 'Price', label: 'Preise' },
    { kind: 'PluginBinding', label: 'Plugin-Bindungen' },
    { kind: 'McpServer', label: 'MCP-Server' },
    { kind: 'McpGrant', label: 'MCP-Freigaben' },
  ];

  const isGitops = () => roles().some((b) => b.role === 'gitops' && !b.organization && !b.team);
  const canExport = $derived(canReadGlobal());
  const canApply = $derived(isGlobalAdmin() || isGitops());

  // ------------------------------------------------------------ Export
  type Scope = 'all' | 'kinds' | 'organization' | 'team' | 'resource';
  const q = new URLSearchParams(route.search);
  const preKinds = (q.get('kinds') ?? '').split(',').filter(Boolean) as Kind[];
  let scope = $state<Scope>(q.get('team') ? 'team' : q.get('organization') ? 'organization' : q.get('name') ? 'resource' : preKinds.length ? 'kinds' : 'all');
  let kinds = $state<Kind[]>(preKinds.length ? preKinds : ['ModelGroup', 'Deployment']);
  let org = $state(q.get('organization') ?? '');
  let team = $state(q.get('team') ?? '');
  let resKind = $state<Kind>(preKinds[0] ?? 'ModelGroup');
  let resName = $state(q.get('name') ?? '');
  let exportKeys = $state<Schemas['ApiKeyMode']>(q.get('apiKeys') === 'ignore' ? 'ignore' : 'include');
  let preview = $state<string | null>(null);
  let exporting = $state(false);
  let exportError = $state<string | null>(null);

  function selection(): ExportSelection | null {
    switch (scope) {
      case 'all':
        return {};
      case 'kinds':
        return kinds.length ? { kinds } : null;
      case 'organization':
        return org ? { organization: org } : null;
      case 'team':
        return team ? { team } : null;
      case 'resource':
        return resName.trim() ? { kinds: [resKind], name: resName.trim() } : null;
    }
  }

  function fileName(sel: ExportSelection): string {
    const part = sel.team ? `team-${sel.team}` : sel.organization ? `org-${sel.organization}` : sel.name ? `${sel.kinds?.[0]}-${sel.name}` : sel.kinds?.length ? sel.kinds.join('-') : 'gesamt';
    return `aigw-config-${part.toLowerCase().replace(/[^a-z0-9._-]+/g, '-')}-${new Date().toISOString().slice(0, 10)}.yaml`;
  }

  async function doExport(save: boolean) {
    exportError = null;
    const sel = selection();
    if (!sel) {
      exportError = 'Bitte die Auswahl vervollständigen.';
      return;
    }
    exporting = true;
    try {
      const text = await exportConfig(exportKeys, sel);
      preview = text;
      if (save) {
        download(fileName(sel), text);
        toast('success', 'Export erstellt', 'YAML-Datei heruntergeladen (ohne Geheimwerte).');
      }
    } catch (e) {
      handleError(e, { action: 'Export', form: (m) => (exportError = m) });
    } finally {
      exporting = false;
    }
  }
  // Vorbelegung aus der URL (z. B. „Als YAML“ auf einer Detailseite): Vorschau sofort laden.
  if ([...q.keys()].some((k) => ['kinds', 'name', 'organization', 'team'].includes(k)) && canReadGlobal()) void doExport(false);

  function toggleKind(k: Kind, on: boolean) {
    kinds = on ? [...new Set([...kinds, k])] : kinds.filter((x) => x !== k);
  }

  // ------------------------------------------------------------ Import
  let yaml = $state('');
  let prune = $state(false);
  let applyKeys = $state<Schemas['ApiKeyMode']>('include');
  let report = $state<Schemas['ApplyReport'] | null>(null);
  let planned = $state<string | null>(null);
  let busy = $state<'plan' | 'apply' | null>(null);
  let error = $state<string | null>(null);
  let issues = $state<Schemas['Issue'][]>([]);
  let confirmOpen = $state(false);
  let editor = $state<YamlEditor>();

  const key = () => `${prune}|${applyKeys}|${yaml}`;
  const stale = $derived(planned !== `${prune}|${applyKeys}|${yaml}`);
  const deletes = $derived(report?.plan.summary.delete ?? 0);
  const changes = $derived(report ? report.plan.summary.create + report.plan.summary.update + report.plan.summary.delete : 0);
  const errorLines = $derived(issues.map((i) => i.line).filter((l): l is number => typeof l === 'number'));

  async function loadFile(e: Event) {
    const file = (e.currentTarget as HTMLInputElement).files?.[0];
    if (file) yaml = await file.text();
  }

  function takeExport() {
    if (preview === null) return;
    yaml = preview;
    report = null;
    issues = [];
    document.getElementById('gitops-yaml')?.focus();
  }

  async function plan() {
    error = null;
    issues = [];
    if (!yaml.trim()) {
      error = 'Bitte Manifeste einfügen oder eine Datei wählen.';
      return;
    }
    busy = 'plan';
    const k = key();
    try {
      report = await applyConfig(yaml, { dryRun: true, prune, apiKeys: applyKeys });
      planned = k;
    } catch (e) {
      report = null;
      planned = null;
      if (e instanceof ApiError && e.problem?.issues?.length) {
        issues = e.problem.issues;
        error = `Manifest ungültig – ${issues.length} Fehler, nichts angewendet.`;
      } else {
        handleError(e, { action: 'Trockenlauf', form: (m) => (error = m) });
      }
    } finally {
      busy = null;
    }
  }

  async function apply(): Promise<string | null> {
    busy = 'apply';
    try {
      const r = await applyConfig(yaml, { dryRun: false, prune, apiKeys: applyKeys });
      report = r;
      planned = null;
      toast('success', r.applied ? 'Konfiguration angewendet' : 'Keine Änderungen', r.applied ? `Neue Konfigurationsversion ${r.configVersion}.` : undefined);
      return null;
    } catch (e) {
      if (e instanceof ApiError && e.problem?.issues?.length) {
        issues = e.problem.issues;
        error = `Manifest ungültig – ${issues.length} Fehler, nichts angewendet.`;
        return null;
      }
      return problemText(e);
    } finally {
      busy = null;
    }
  }

  const issueWhere = (i: Schemas['Issue']) => [i.document ? `Dokument ${i.document}` : null, i.kind && i.name ? `${i.kind} „${i.name}“` : i.kind].filter(Boolean).join(' · ');
</script>

<PageHeader
  title="YAML-Import/-Export"
  description="Ressourcen als Manifeste (aigw/v1alpha1) – exportieren, versionieren, prüfen und anwenden. Dieselbe Logik wie „aigw config export|diff|apply“."
>
  {#snippet badge()}{#if auth.readOnly}<Badge tone="info">Nur-YAML-Modus</Badge>{/if}{/snippet}
</PageHeader>

<div class="grid gap-4 2xl:grid-cols-2">
  <Card title="Export" description="Geheimwerte nie im Klartext: Zugangsdaten und Secrets als valueFrom-Verweise, API-Keys ohne Klartext." testid="gitops-export-card">
    {#if !canExport}
      <p class="flex items-center gap-2 text-sm text-fg-muted"><Icon name="lock" size={14} /> Export braucht eine globale Rolle (admin, viewer, auditor oder gitops).</p>
    {:else}
      <div class="flex flex-col gap-4">
        <div class="grid gap-3 sm:grid-cols-2">
          <Field label="Umfang">
            {#snippet children({ id })}
              <Select
                {id}
                bind:value={scope}
                items={[
                  { value: 'all', label: 'Alles' },
                  { value: 'kinds', label: 'Nach Art' },
                  { value: 'organization', label: 'Organisation', hint: 'mit Teams, Keys, Bindungen' },
                  { value: 'team', label: 'Team', hint: 'mit Keys, Bindungen' },
                  { value: 'resource', label: 'Einzelne Ressource' },
                ]}
              />
            {/snippet}
          </Field>
          <Field label="API-Keys">
            {#snippet children({ id })}
              <Select
                {id}
                bind:value={exportKeys}
                items={[
                  { value: 'include', label: 'mit API-Keys (ohne Klartext)' },
                  { value: 'ignore', label: 'ohne API-Keys' },
                ]}
              />
            {/snippet}
          </Field>
        </div>
        {#if scope === 'kinds'}
          <fieldset class="grid grid-cols-2 gap-2 sm:grid-cols-3" data-testid="export-kinds">
            <legend class="sr-only">Arten</legend>
            {#each KINDS as k (k.kind)}
              <label class="flex items-center gap-2 text-sm">
                <Checkbox.Root checked={kinds.includes(k.kind)} onCheckedChange={(v) => toggleKind(k.kind, v)} class={checkbox} data-kind={k.kind}>
                  {#snippet children({ checked })}{#if checked}<Icon name="check" size={12} />{/if}{/snippet}
                </Checkbox.Root>
                {k.label}
              </label>
            {/each}
          </fieldset>
        {:else if scope === 'organization'}
          <Field label="Organisation" hint="Katalog-Ressourcen (Modellgruppen, Deployments …) gehören zu keiner Organisation.">
            {#snippet children({ id, describedby })}
              <Select {id} {describedby} bind:value={org} items={catalog.orgs.map((o) => ({ value: o.metadata.name, label: o.metadata.name }))} placeholder="Organisation wählen …" />
            {/snippet}
          </Field>
        {:else if scope === 'team'}
          <Field label="Team">
            {#snippet children({ id })}
              <Select {id} bind:value={team} items={catalog.teams.map((t) => ({ value: t.metadata.name, label: t.metadata.name, hint: t.spec.organization }))} placeholder="Team wählen …" />
            {/snippet}
          </Field>
        {:else if scope === 'resource'}
          <div class="grid gap-3 sm:grid-cols-2">
            <Field label="Art">
              {#snippet children({ id })}
                <Select {id} bind:value={resKind} items={KINDS.map((k) => ({ value: k.kind, label: k.label }))} />
              {/snippet}
            </Field>
            <Field label="Name">
              {#snippet children({ id, describedby })}
                <Input {id} aria-describedby={describedby} bind:value={resName} placeholder="z. B. gpt-4o" data-testid="export-name" />
              {/snippet}
            </Field>
          </div>
        {/if}
        {#if scope !== 'all'}
          <p class="flex gap-2 text-xs text-fg-muted"><Icon name="info" size={12} class="mt-0.5" />Teil-Export: beim Anwenden nicht „Prune“ wählen – sonst würde alles außerhalb der Auswahl gelöscht.</p>
        {/if}
        <FormError message={exportError} />
        <div class="flex flex-wrap gap-2">
          <Button icon="eye" loading={exporting} disabled={exporting} onclick={() => doExport(false)} data-testid="gitops-preview">Vorschau</Button>
          <Button variant="primary" icon="download" disabled={exporting} onclick={() => doExport(true)} data-testid="gitops-export">YAML herunterladen</Button>
          {#if preview !== null && canApply}<Button icon="arrow-down" onclick={takeExport} data-testid="gitops-take">In Import übernehmen</Button>{/if}
        </div>
        {#if preview !== null}
          <div data-testid="gitops-export-preview"><CodeBlock code={preview} lang="yaml" title="Export" maxHeight="max-h-[32rem]" /></div>
        {/if}
      </div>
    {/if}
  </Card>

  <Card title="Import" description="Erst Trockenlauf mit Diff, dann Anwenden – alles in einer Transaktion (alles oder nichts)." testid="gitops-import-card">
    {#if !canApply}
      <p class="flex items-center gap-2 text-sm text-fg-muted"><Icon name="lock" size={14} /> Anwenden (auch als Trockenlauf) ist globalen Admins und der Rolle gitops vorbehalten.</p>
    {:else}
      <div class="flex flex-col gap-3">
        <p class="text-sm text-fg-muted">
          Über die API sind als <code class="font-mono">valueFrom</code> nur Umgebungsvariablen mit Präfix <code class="font-mono">AIGW_SECRET_</code> und Dateien in
          <code class="font-mono">gitops.secret_dir</code> erlaubt – Klartext-Geheimnisse gehören nicht ins Manifest.
        </p>
        <label class="text-sm font-medium" for="gitops-yaml">Manifeste (YAML)</label>
        <YamlEditor bind:this={editor} bind:value={yaml} {errorLines} placeholder={'apiVersion: aigw/v1alpha1\nkind: ModelGroup\nmetadata: {name: gpt-4o}\nspec: {routing: weighted}'} />
        <div class="flex flex-wrap items-center gap-x-5 gap-y-2">
          <label class="flex items-center gap-2 text-sm">
            <span class="sr-only">YAML-Datei laden</span>
            <input type="file" accept=".yaml,.yml,application/yaml,text/yaml" onchange={loadFile} class="text-sm text-fg-muted file:mr-2 file:rounded-md file:border file:border-line file:bg-surface file:px-2 file:py-1 file:text-sm file:text-fg" />
          </label>
          <label class="flex items-center gap-2 text-sm">
            <Checkbox.Root bind:checked={prune} class={checkbox} data-testid="gitops-prune">
              {#snippet children({ checked })}{#if checked}<Icon name="check" size={12} />{/if}{/snippet}
            </Checkbox.Root>
            Prune <span class="text-xs text-fg-muted">(nicht im Manifest = löschen)</span>
          </label>
          <div class="w-48">
            <Select
              size="sm"
              label="API-Keys im Manifest"
              bind:value={applyKeys}
              items={[
                { value: 'include', label: 'API-Keys anwenden' },
                { value: 'ignore', label: 'API-Keys ignorieren' },
              ]}
            />
          </div>
        </div>
        <FormError message={error} />
        {#if issues.length}
          <ul class="flex flex-col divide-y divide-line rounded-md border border-danger-fg/30 bg-danger-bg text-sm" data-testid="gitops-issues">
            {#each issues as i, n (n)}
              <li class="flex flex-wrap items-baseline gap-x-2 gap-y-1 px-3 py-2 text-danger-fg">
                {#if i.line}
                  <button type="button" class="font-mono font-semibold underline underline-offset-2" onclick={() => editor?.gotoLine(i.line!)} data-testid="gitops-issue-line">Zeile {i.line}{#if i.column}:{i.column}{/if}</button>
                {/if}
                {#if issueWhere(i)}<span class="text-xs">{issueWhere(i)}</span>{/if}
                <span class="w-full text-fg">{i.message}</span>
              </li>
            {/each}
          </ul>
        {/if}
        <div class="flex flex-wrap items-center gap-2">
          <Button icon="search" loading={busy === 'plan'} disabled={!!busy} onclick={plan} data-testid="gitops-plan">Trockenlauf</Button>
          <Button variant="primary" icon="check" disabled={!!busy || stale || !report || report.applied} onclick={() => (confirmOpen = true)} data-testid="gitops-apply">Anwenden …</Button>
          {#if report && stale && !report.applied}<span class="text-xs text-warning-fg">Eingabe geändert – Trockenlauf wiederholen.</span>{/if}
          {#if prune && !stale && deletes}<span class="text-xs text-danger-fg">Achtung: {deletes} Ressource(n) werden gelöscht.</span>{/if}
        </div>
        {#if report}<PlanView {report} />{/if}
      </div>
    {/if}
  </Card>
</div>

<ConfirmDialog
  bind:open={confirmOpen}
  title="Konfiguration anwenden?"
  confirmLabel="Anwenden"
  icon="check"
  danger={deletes > 0}
  requireText={deletes > 0 ? 'ANWENDEN' : undefined}
  onconfirm={apply}
  testid="gitops-confirm"
>
  {#if report}
    <p class="text-base">
      Basis ist Konfigurationsversion <span class="num">{report.plan.baseVersion}</span>. Es werden
      <strong class="num">{report.plan.summary.create}</strong> Ressource(n) angelegt, <strong class="num">{report.plan.summary.update}</strong> geändert und
      <strong class="num">{deletes}</strong> gelöscht ({changes} Änderung{changes === 1 ? '' : 'en'}).
    </p>
    {#if deletes > 0}<p class="mt-2 text-sm text-danger-fg">Löschungen sind nicht umkehrbar. Zum Bestätigen „ANWENDEN“ eingeben.</p>{/if}
    <p class="mt-2 text-sm text-fg-muted">Der Server prüft und plant beim Anwenden erneut; ändert sich die Konfiguration währenddessen, bricht er ohne Änderung ab (409).</p>
  {/if}
</ConfirmDialog>
