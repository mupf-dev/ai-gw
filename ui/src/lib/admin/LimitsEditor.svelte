<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Limit-Editor (LimitSpec[]): Kennzahl, Grenze, Fenster (gleitend second … 30d oder Kalender, F25) und optional
  // eine Modellgruppe. Leere Grenze = Zeile wird beim Speichern verworfen.
  import Icon from '../ui/Icon.svelte';
  import Select from '../ui/Select.svelte';
  import { btn, input } from '../ui/styles';
  import { METRICS, WINDOWS, type LimitRow as Row } from './limits';

  let { value = $bindable([]), groups = [] }: { value?: Row[]; groups?: string[] } = $props();
  const ALL = '__all__';
</script>

<div class="flex flex-col gap-2" data-testid="limits-editor">
  {#each value as r, i (i)}
    <div class="grid grid-cols-2 gap-2 rounded-lg border border-line bg-surface-2 p-2 sm:grid-cols-[7rem_1fr_1fr_1fr_auto]">
      <input class={input} type="text" inputmode="numeric" aria-label="Grenze {i + 1}" bind:value={r.limit} placeholder="600" />
      <Select label="Kennzahl {i + 1}" bind:value={r.metric} items={METRICS} />
      <Select label="Fenster {i + 1}" bind:value={r.window} items={WINDOWS} />
      <Select
        label="Modellgruppe {i + 1}"
        value={r.modelGroup || ALL}
        onchange={(v) => (r.modelGroup = v === ALL ? '' : v)}
        items={[{ value: ALL, label: 'alle Modelle' }, ...groups.map((g) => ({ value: g, label: g }))]}
      />
      <button type="button" class={btn('ghost', 'icon')} aria-label="Limit {i + 1} entfernen" onclick={() => (value = value.filter((_, j) => j !== i))}><Icon name="x" size={14} /></button>
    </div>
  {/each}
  <div>
    <button type="button" class={btn('secondary', 'sm')} onclick={() => (value = [...value, { metric: 'requests', window: 'minute', limit: '', modelGroup: '' }])} data-testid="add-limit">
      <Icon name="plus" size={14} /> Limit hinzufügen
    </button>
  </div>
</div>
