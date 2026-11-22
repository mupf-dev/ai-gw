<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // GitOps-Plan (Trockenlauf/Ergebnis von config:apply) als Diff: Zusammenfassung je Aktion, Warnungen und je
  // Ressource die Aktion mit geänderten Feldern (vorher → nachher; Geheimnisse nur als „geändert“).
  import type { Schemas } from '../../lib/api/client';
  import Badge, { type Tone } from '../../lib/ui/Badge.svelte';
  import Icon from '../../lib/ui/Icon.svelte';

  let { report }: { report: Schemas['ApplyReport'] } = $props();
  let showUnchanged = $state(false);

  const ACTION: Record<Schemas['ChangeAction'], { label: string; tone: Tone; sign: string }> = {
    create: { label: 'anlegen', tone: 'success', sign: '+' },
    update: { label: 'ändern', tone: 'warning', sign: '~' },
    delete: { label: 'löschen', tone: 'danger', sign: '−' },
    unchanged: { label: 'unverändert', tone: 'neutral', sign: '=' },
    retain: { label: 'bleibt', tone: 'outline', sign: '·' },
    skip: { label: 'übersprungen', tone: 'info', sign: '!' },
  };
  const plan = $derived(report.plan);
  const visible = $derived(plan.changes.filter((c) => showUnchanged || (c.action !== 'unchanged' && c.action !== 'retain')));
  const fmt = (v: unknown) => (v === undefined || v === null ? '–' : typeof v === 'string' ? v : JSON.stringify(v));
</script>

<div class="flex flex-col gap-3" data-testid="apply-plan">
  <div class="flex flex-wrap items-center gap-2">
    {#each Object.entries(plan.summary) as [k, n] (k)}
      {@const a = ACTION[k as Schemas['ChangeAction']]}
      <Badge tone={n ? a.tone : 'outline'}><span class="num">{n}</span> {a.label}</Badge>
    {/each}
    <span class="ml-auto text-xs text-fg-muted">Basis: Konfigurationsversion <span class="num">{plan.baseVersion}</span>{report.applied ? ` → ${report.configVersion}` : ''}</span>
  </div>
  {#each plan.warnings as w, i (i)}
    <p class="flex gap-2 rounded-md bg-warning-bg px-3 py-2 text-sm text-warning-fg"><Icon name="alert" size={14} class="mt-0.5" />{w}</p>
  {/each}
  <label class="flex items-center gap-2 text-sm text-fg-muted"><input type="checkbox" bind:checked={showUnchanged} class="accent-accent" /> Unveränderte anzeigen</label>
  <ul class="divide-y divide-line rounded-lg border border-line font-mono text-sm">
    {#each visible as c, i (i)}
      {@const a = ACTION[c.action]}
      <li class="px-3 py-2">
        <div class="flex items-center gap-2">
          <span class={['w-3 text-center font-bold', a.tone === 'success' && 'text-success-fg', a.tone === 'danger' && 'text-danger-fg', a.tone === 'warning' && 'text-warning-fg']} aria-hidden="true">{a.sign}</span>
          <span class="text-fg-muted">{c.kind}</span>
          <span class="font-medium text-fg">{c.name}</span>
          <Badge tone={a.tone}>{a.label}</Badge>
          {#if c.note}<span class="truncate font-sans text-xs text-fg-muted">{c.note}</span>{/if}
        </div>
        {#if c.fields?.length}
          <ul class="mt-1.5 flex flex-col gap-0.5 pl-5 text-xs">
            {#each c.fields as fc (fc.path)}
              <li class="flex flex-wrap gap-x-2">
                <span class="text-fg-muted">{fc.path}:</span>
                {#if fc.secret}<span class="text-warning-fg">Geheimnis geändert</span>
                {:else}
                  <span class="text-danger-fg line-through decoration-1">{fmt(fc.before)}</span>
                  <span class="text-fg-subtle">→</span>
                  <span class="text-success-fg">{fmt(fc.after)}</span>
                {/if}
              </li>
            {/each}
          </ul>
        {/if}
      </li>
    {:else}
      <li class="px-3 py-4 text-center font-sans text-fg-muted">Keine Änderungen.</li>
    {/each}
  </ul>
</div>
