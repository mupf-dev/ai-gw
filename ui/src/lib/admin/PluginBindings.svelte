<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Plugin-Bindungen auf einer Ebene (nur lesen) mit wirksamem Fehlermodus (F148): `closed` = bei Fehler/Timeout
  // ablehnen, `open` = überspringen; Herkunft Bindung bzw. Plugin-Standard. Pflege folgt mit der Guardrails-Seite.
  import type { S } from '../api/resources';
  import { pluginBindings } from '../api/observe';
  import Badge from '../ui/Badge.svelte';
  import EmptyState from '../ui/EmptyState.svelte';
  import AsyncState from './AsyncState.svelte';
  import { Loader } from './errors.svelte';

  let { level, target }: { level: S['ScopeLevel']; target?: string } = $props();
  const data = new Loader(async () => (await pluginBindings(level, target)).items);
</script>

<div data-testid="plugin-bindings">
  <AsyncState loader={data} rows={2}>
    {#snippet children(items)}
      {#if items.length === 0}
        <EmptyState compact icon="shield" title="Keine Plugins auf dieser Ebene" text="Geerbte Bindungen höherer Ebenen (global, Organisation, Team) gelten zusätzlich." />
      {:else}
        <table class="w-full text-left text-base">
          <thead>
            <tr class="border-b border-line bg-surface-2 text-sm text-fg-muted">
              <th class="h-9 px-3 font-medium">Bindung</th>
              <th class="h-9 px-3 font-medium">Plugin</th>
              <th class="h-9 px-3 font-medium">Fehlermodus</th>
              <th class="hidden h-9 px-3 font-medium md:table-cell">Reihenfolge</th>
              <th class="h-9 px-3 font-medium">Aktiv</th>
            </tr>
          </thead>
          <tbody>
            {#each items as b (b.metadata.name)}
              {@const fm = b.effective?.failMode}
              <tr class="border-b border-line last:border-0" data-testid="binding-row">
                <td class="px-3 py-2 font-mono text-sm">{b.metadata.name}</td>
                <td class="px-3 py-2"><Badge mono>{b.spec.plugin}</Badge></td>
                <td class="px-3 py-2">
                  {#if fm}
                    <Badge tone={fm === 'closed' ? 'warning' : 'info'} dot>{fm === 'closed' ? 'closed – ablehnen' : 'open – überspringen'}</Badge>
                    <span class="ml-1 text-xs text-fg-subtle">{b.effective?.failModeSource === 'binding' ? 'Bindung' : 'Plugin-Standard'}</span>
                  {:else}<span class="text-sm text-fg-muted">–</span>{/if}
                </td>
                <td class="num hidden px-3 py-2 text-sm md:table-cell">{b.spec.order ?? 0}</td>
                <td class="px-3 py-2"><Badge tone={b.spec.enabled === false ? 'neutral' : 'success'} dot>{b.spec.enabled === false ? 'aus' : 'an'}</Badge></td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    {/snippet}
  </AsyncState>
</div>
