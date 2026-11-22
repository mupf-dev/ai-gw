<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Lesende Anzeige der Einstellungen (D9): gesetzte bekannte Felder je Abschnitt, Rest als JSON.
  import CodeBlock from '../../lib/ui/CodeBlock.svelte';
  import { getPath, unknownKeys, type SettingSection } from './settings';

  let { settings, sections }: { settings: Record<string, unknown>; sections: SettingSection[] } = $props();

  const shown = $derived(
    sections
      .map((s) => ({
        ...s,
        set: s.fields
          .map((f) => ({ f, v: getPath(settings, f.path) }))
          .filter((x) => x.v !== undefined && x.v !== null),
      }))
      .filter((s) => s.set.length),
  );
  const extra = $derived(Object.fromEntries(unknownKeys(settings, sections).map((k) => [k, settings[k]])));
  const fmt = (v: unknown, choices?: { value: string; label: string }[]) =>
    typeof v === 'boolean' ? (v ? 'an' : 'aus') : Array.isArray(v) ? v.join(', ') : (choices?.find((c) => c.value === v)?.label ?? String(v));
</script>

<div class="flex flex-col gap-3" data-testid="settings-view">
  <h3 class="label-caps text-fg-subtle">Einstellungen</h3>
  {#if shown.length === 0 && Object.keys(extra).length === 0}
    <p class="text-sm text-fg-muted">Alle Standardwerte.</p>
  {/if}
  {#if shown.length}
    <div class="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
      {#each shown as s (s.id)}
        <div class="rounded-md border border-line p-2.5">
          <div class="mb-1 text-sm font-medium">{s.title}</div>
          <dl class="grid grid-cols-[1fr_auto] gap-x-3 gap-y-0.5 text-sm">
            {#each s.set as x (x.f.path)}
              <dt class="text-fg-muted">{x.f.label}</dt>
              <dd class="num text-right">{fmt(x.v, x.f.choices)}</dd>
            {/each}
          </dl>
        </div>
      {/each}
    </div>
  {/if}
  {#if Object.keys(extra).length}<CodeBlock title="Weitere Einstellungen" code={extra} maxHeight="max-h-40" />{/if}
</div>
