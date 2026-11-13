<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // GitOps über die Admin-API (AIGW-33): Export als YAML-Datei; Apply erst als Trockenlauf (Plan/Diff anzeigen),
  // dann Anwenden desselben Textes. Export: globale Leserolle; Apply (auch Trockenlauf): globaler Admin.
  import { Checkbox } from 'bits-ui';
  import { applyConfig, exportConfig, type Schemas } from '../../lib/api/client';
  import { download, handleError } from '../../lib/admin/errors.svelte';
  import FormError from '../../lib/admin/FormError.svelte';
  import { canReadGlobal, isGlobalAdmin } from '../../lib/rbac.svelte';
  import { toast } from '../../lib/toast.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import Icon from '../../lib/ui/Icon.svelte';
  import Select from '../../lib/ui/Select.svelte';
  import { checkbox, textarea } from '../../lib/ui/styles';
  import PlanView from './PlanView.svelte';

  let exportKeys = $state<Schemas['ApiKeyMode']>('include');
  let exporting = $state(false);
  let yaml = $state('');
  let prune = $state(false);
  let applyKeys = $state<Schemas['ApiKeyMode']>('include');
  let report = $state<Schemas['ApplyReport'] | null>(null);
  let planned = $state<string | null>(null);
  let busy = $state<'plan' | 'apply' | null>(null);
  let error = $state<string | null>(null);

  const stale = $derived(planned !== `${prune}|${applyKeys}|${yaml}`);

  async function doExport() {
    exporting = true;
    try {
      const text = await exportConfig(exportKeys);
      download(`aigw-config-${new Date().toISOString().slice(0, 10)}.yaml`, text);
      toast('success', 'Export erstellt', 'YAML-Datei heruntergeladen (ohne Geheimwerte).');
    } catch (e) {
      handleError(e, { action: 'Export' });
    } finally {
      exporting = false;
    }
  }

  async function loadFile(e: Event) {
    const file = (e.currentTarget as HTMLInputElement).files?.[0];
    if (file) yaml = await file.text();
  }

  async function run(dryRun: boolean) {
    error = null;
    if (!yaml.trim()) {
      error = 'Bitte Manifeste einfügen oder eine Datei wählen.';
      return;
    }
    busy = dryRun ? 'plan' : 'apply';
    const key = `${prune}|${applyKeys}|${yaml}`;
    try {
      const r = await applyConfig(yaml, { dryRun, prune, apiKeys: applyKeys });
      report = r;
      planned = dryRun ? key : null;
      if (!dryRun) toast('success', r.applied ? 'Konfiguration angewendet' : 'Keine Änderungen', r.applied ? `Neue Konfigurationsversion ${r.configVersion}.` : undefined);
    } catch (e) {
      report = null;
      planned = null;
      handleError(e, { action: dryRun ? 'Trockenlauf' : 'Anwenden', form: (m) => (error = m) });
    } finally {
      busy = null;
    }
  }
</script>

<Card title="GitOps" description="Konfiguration als YAML-Manifeste (aigw/v1alpha1) – exportieren, versionieren, prüfen und anwenden.">
  <div class="flex flex-col gap-6">
    <section class="flex flex-col gap-2">
      <h3 class="text-base font-semibold">Export</h3>
      <p class="text-sm text-fg-muted">Alle Ressourcen als Mehrfachdokument; Geheimwerte nie im Klartext (Zugangsdaten als <code class="font-mono">valueFrom</code>-Verweise).</p>
      <div class="flex flex-wrap items-center gap-2">
        <div class="w-56">
          <Select
            size="sm"
            label="API-Keys im Export"
            bind:value={exportKeys}
            items={[
              { value: 'include', label: 'mit API-Keys (ohne Klartext)' },
              { value: 'ignore', label: 'ohne API-Keys' },
            ]}
          />
        </div>
        <Button icon="download" loading={exporting} disabled={exporting || !canReadGlobal()} onclick={doExport} data-testid="gitops-export">YAML herunterladen</Button>
        {#if !canReadGlobal()}<span class="text-xs text-fg-muted">braucht eine globale Rolle</span>{/if}
      </div>
    </section>

    <section class="flex flex-col gap-3 border-t border-line pt-5">
      <h3 class="text-base font-semibold">Apply</h3>
      {#if !isGlobalAdmin()}
        <p class="flex items-center gap-2 text-sm text-fg-muted"><Icon name="lock" size={14} /> Anwenden (auch als Trockenlauf) ist globalen Admins vorbehalten.</p>
      {:else}
        <p class="text-sm text-fg-muted">
          Erst Trockenlauf: zeigt, was angelegt, geändert und gelöscht würde. „Anwenden“ schreibt alles in <strong>einer</strong> Transaktion (alles oder nichts). Über die API sind
          nur Umgebungsvariablen mit Präfix <code class="font-mono">AIGW_SECRET_</code> als <code class="font-mono">valueFrom</code> erlaubt.
        </p>
        <label class="text-sm font-medium" for="gitops-yaml">Manifeste (YAML)</label>
        <textarea id="gitops-yaml" class={[textarea, 'min-h-48']} bind:value={yaml} spellcheck="false" placeholder={'apiVersion: aigw/v1alpha1\nkind: ModelGroup\nmetadata: {name: gpt-4o}\nspec: {routing: weighted}'} data-testid="gitops-yaml"></textarea>
        <div class="flex flex-wrap items-center gap-x-5 gap-y-2">
          <label class="flex items-center gap-2 text-sm">
            <span class="sr-only">YAML-Datei laden</span>
            <input type="file" accept=".yaml,.yml,application/yaml,text/yaml" onchange={loadFile} class="text-sm text-fg-muted file:mr-2 file:rounded-md file:border file:border-line file:bg-surface file:px-2 file:py-1 file:text-sm file:text-fg" />
          </label>
          <label class="flex items-center gap-2 text-sm">
            <Checkbox.Root bind:checked={prune} class={checkbox}>
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
        <div class="flex flex-wrap items-center gap-2">
          <Button icon="search" loading={busy === 'plan'} disabled={!!busy} onclick={() => run(true)} data-testid="gitops-plan">Trockenlauf</Button>
          <Button variant="primary" icon="check" loading={busy === 'apply'} disabled={!!busy || stale || !report || report.applied} onclick={() => run(false)} data-testid="gitops-apply">Anwenden</Button>
          {#if report && stale && !report.applied}<span class="text-xs text-warning-fg">Eingabe geändert – Trockenlauf wiederholen.</span>{/if}
          {#if prune && !stale && report?.plan.summary.delete}<span class="text-xs text-danger-fg">Achtung: {report.plan.summary.delete} Ressource(n) werden gelöscht.</span>{/if}
        </div>
        {#if report}<PlanView {report} />{/if}
      {/if}
    </section>
  </div>
</Card>
