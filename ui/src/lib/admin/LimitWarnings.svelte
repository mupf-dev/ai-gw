<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Hinweise zu Limits, die mit dem gewählten Zähler-Backend nur ungenau wirken (GET /usage/limit-warnings, F29),
  // z. B. Sekundenlimits mit counters.backend = postgres-sync. Ohne Hinweise unsichtbar; Fehler werden verschluckt
  // (rein informativ). `name`: nur Hinweise zu diesem Objekt.
  import { limitWarnings } from '../api/observe';
  import Icon from '../ui/Icon.svelte';
  import { Loader } from './errors.svelte';

  let { kind, name }: { kind?: string; name?: string } = $props();
  const data = new Loader(async () => {
    try {
      const r = await limitWarnings();
      return { ...r, warnings: r.warnings.filter((w) => (!kind || w.kind === kind) && (!name || w.name === name)) };
    } catch {
      return null;
    }
  });
</script>

{#if data.data && data.data.warnings.length}
  <div class="flex flex-col gap-1 rounded-md border border-warning-fg/30 bg-warning-bg px-3 py-2 text-sm text-warning-fg" role="status" data-testid="limit-warnings">
    <span class="flex items-center gap-1.5 font-semibold"><Icon name="alert" size={14} /> Limits wirken mit dem Zähler-Backend „{data.data.counterBackend}“ nur ungenau</span>
    <ul class="list-disc pl-6">
      {#each data.data.warnings as w, i (i)}<li><span class="font-mono">{w.kind} {w.name} · {w.field}</span>: {w.message}</li>{/each}
    </ul>
  </div>
{/if}
